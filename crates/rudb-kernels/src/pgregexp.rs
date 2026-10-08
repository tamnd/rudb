//! The regular expression operators and functions of a PostgreSQL session, over the PostgreSQL
//! flavour of `rudb-regex`.
//!
//! The binder writes `~` and `~*` as `__rudb_pg_regex_match`, with the flag `i` for `~*`, and each
//! of the `regexp_*` functions and `substring(text from pattern)` as a kernel of its own that takes
//! every parameter, with the default of each one the call leaves out. `regexp_matches` and
//! `regexp_split_to_table` give the list of their rows, and the unnest that the binder puts around
//! them gives the rows one at a time, in a select list and in `FROM`.
//!
//! `SIMILAR TO` is `~` over what `similar_to_escape` makes of the pattern, as in PostgreSQL, and
//! `substring(text similar pattern escape escape)` is `substring` from a pattern over it.
//!
//! The pattern and the flags are compiled once for a query where they are literals, and once for
//! each run of rows that have the same ones where they are not. A pattern that does not compile
//! gives its error from the first row that reaches it, as in PostgreSQL, and not when the query is
//! prepared. The positions that the functions take and give count characters and not bytes.

use rudb_common::{Error, LogicalType, Result, SqlState, Value};
use rudb_regex::{Captures, PgFlags, Regex};
use rudb_vector::{Data, Vector};

use crate::scalar::{finish, over_valid};
use crate::shape::nulls_of;

/// One of the calls of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Function {
    /// `text ~ pattern`, with the flag letters as a third argument.
    Match,
    /// `regexp_like(text, pattern, flags)`.
    Like,
    /// `regexp_match(text, pattern, flags)`.
    First,
    /// `regexp_matches(text, pattern, flags)`, as the list of its rows.
    Every,
    /// `regexp_count(text, pattern, start, flags)`.
    Count,
    /// `regexp_instr(text, pattern, start, n, endoption, flags, subexpr)`.
    Instr,
    /// `regexp_substr(text, pattern, start, n, flags, subexpr)`.
    Substr,
    /// `regexp_replace(text, pattern, replacement, flags)`, which replaces every match with the
    /// flag `g` and the first one without it.
    Replace,
    /// `regexp_replace(text, pattern, replacement, start, n, flags)`, which replaces match `n`, or
    /// every match for an `n` of 0.
    ReplaceAt,
    /// `regexp_split_to_array(text, pattern, flags)`.
    SplitArray,
    /// `regexp_split_to_table(text, pattern, flags)`, as the list of its rows.
    SplitTable,
    /// `substring(text from pattern)`, which gives the first group, or the whole match for a
    /// pattern with no groups.
    Substring,
}

/// A parameter of one of the functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parameter {
    /// The text that is searched.
    Text,
    Pattern,
    /// What `regexp_replace` writes for a match.
    Replacement,
    /// The flag letters, which are none by default.
    Flags,
    /// The character that the search starts at, from 1, which is the default.
    Start,
    /// Which match the call gives, from 1, which is the default.
    N,
    /// 0, the default, for the position of the match, and 1 for the position after it.
    EndOption,
    /// Which group the call gives, or 0, the default, for the whole match.
    Subexpr,
}

use Parameter::{EndOption, Flags, N, Pattern, Replacement, Start, Subexpr, Text};

/// The kernel and the SQL name of each function. The operator has no SQL name. Of the two forms of
/// `regexp_replace`, a name finds the first, and the binder takes the second for an integer as the
/// fourth argument.
const FUNCTIONS: [(Function, &str, &str); 12] = [
    (Function::Match, "__rudb_pg_regex_match", ""),
    (Function::Like, "__rudb_pg_regexp_like", "regexp_like"),
    (Function::First, "__rudb_pg_regexp_match", "regexp_match"),
    (Function::Every, "__rudb_pg_regexp_matches", "regexp_matches"),
    (Function::Count, "__rudb_pg_regexp_count", "regexp_count"),
    (Function::Instr, "__rudb_pg_regexp_instr", "regexp_instr"),
    (Function::Substr, "__rudb_pg_regexp_substr", "regexp_substr"),
    (Function::Replace, "__rudb_pg_regexp_replace", "regexp_replace"),
    (Function::ReplaceAt, "__rudb_pg_regexp_replace_at", "regexp_replace"),
    (Function::SplitArray, "__rudb_pg_regexp_split_to_array", "regexp_split_to_array"),
    (Function::SplitTable, "__rudb_pg_regexp_split_to_table", "regexp_split_to_table"),
    (Function::Substring, "__rudb_pg_substring", "substring"),
];

impl Function {
    /// The function that the plan records as `kernel`.
    #[must_use]
    pub fn of_kernel(kernel: &str) -> Option<Self> {
        FUNCTIONS.iter().find(|(_, held, _)| *held == kernel).map(|&(function, ..)| function)
    }

    /// The function that SQL calls `name`, found without case.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        let found =
            FUNCTIONS.iter().find(|(.., sql)| !sql.is_empty() && sql.eq_ignore_ascii_case(name));
        found.map(|&(function, ..)| function)
    }

    /// The name that the plan records.
    #[must_use]
    pub fn kernel(self) -> &'static str {
        FUNCTIONS.iter().find(|(held, ..)| *held == self).map_or("", |(_, kernel, _)| kernel)
    }

    /// The name that SQL calls the function, which its errors give.
    #[must_use]
    pub fn name(self) -> &'static str {
        FUNCTIONS.iter().find(|(held, ..)| *held == self).map_or("", |(.., sql)| sql)
    }

    /// Every parameter, in order. A call gives the first two and any number of the others.
    #[must_use]
    pub fn parameters(self) -> &'static [Parameter] {
        match self {
            Self::Match | Self::Like | Self::First | Self::Every => &[Text, Pattern, Flags],
            Self::SplitArray | Self::SplitTable => &[Text, Pattern, Flags],
            Self::Substring => &[Text, Pattern],
            Self::Replace => &[Text, Pattern, Replacement, Flags],
            Self::ReplaceAt => &[Text, Pattern, Replacement, Start, N, Flags],
            Self::Count => &[Text, Pattern, Start, Flags],
            Self::Instr => &[Text, Pattern, Start, N, EndOption, Flags, Subexpr],
            Self::Substr => &[Text, Pattern, Start, N, Flags, Subexpr],
        }
    }

    /// The type of what the function gives.
    #[must_use]
    pub fn returns(self) -> LogicalType {
        let text = || LogicalType::List(Box::new(LogicalType::Varchar));
        match self {
            Self::Match | Self::Like => LogicalType::Boolean,
            Self::First => text(),
            Self::Every => LogicalType::List(Box::new(text())),
            Self::SplitArray | Self::SplitTable => text(),
            Self::Count | Self::Instr => LogicalType::Integer,
            Self::Substr | Self::Replace | Self::ReplaceAt | Self::Substring => {
                LogicalType::Varchar
            }
        }
    }

    /// Whether the function gives a set of rows, which the binder unnests.
    #[must_use]
    pub fn is_set(self) -> bool {
        matches!(self, Self::Every | Self::SplitTable)
    }

    /// Where `parameter` is among the arguments of the kernel.
    fn at(self, parameter: Parameter) -> Option<usize> {
        self.parameters().iter().position(|&held| held == parameter)
    }
}

impl Parameter {
    /// Whether the parameter is a string, rather than an `int4`.
    #[must_use]
    pub fn is_text(self) -> bool {
        matches!(self, Text | Pattern | Replacement | Flags)
    }

    /// Whether every call gives the parameter.
    #[must_use]
    pub fn is_required(self) -> bool {
        matches!(self, Text | Pattern | Replacement)
    }

    /// The value of the parameter where the call leaves it out.
    #[must_use]
    pub fn default_value(self) -> Value {
        match self {
            Text | Pattern | Replacement | Flags => Value::Varchar(String::new()),
            Start | N => Value::Integer(1),
            EndOption | Subexpr => Value::Integer(0),
        }
    }
}

/// The kernel of `similar_to_escape(pattern, escape)`. The binder gives a backslash as the escape
/// where the call gives none.
pub const SIMILAR_ESCAPE: &str = "__rudb_pg_similar_to_escape";

/// The regular expression that `SIMILAR TO` matches with, as `similar_escape_internal` in
/// `regexp.c` writes it. The pattern must match the whole text, `%` and `_` are `.*` and `.`, and
/// a group does not capture. The escape followed by a double quote splits the pattern into the
/// three parts of `substring`, and the middle part is the one group. Within brackets the text is
/// kept as it is, and the brackets end at the first `]` that is not the first member of the class.
pub fn similar_escape(pattern: &str, escape: &str) -> Result<String> {
    let mut letters = escape.chars();
    let escape = match (letters.next(), letters.next()) {
        (None, _) => None,
        (Some(escape), None) => Some(escape),
        _ => {
            let error = Error::invalid_input("invalid escape string")
                .state(SqlState::INVALID_ESCAPE_SEQUENCE)
                .hint("Escape string must be empty or one character.");
            return Err(error.unplaced());
        }
    };
    // PostgreSQL reads the pattern a byte at a time unless both the escape and the character are
    // more than one byte. The two ways give the same text, except that a character of more than
    // one byte in brackets ends the start of the class only the first way.
    let wide = escape.is_some_and(|escape| escape.len_utf8() > 1);
    let mut out = String::with_capacity(pattern.len() * 3 + 23);
    out.push_str("^(?:");
    let mut escaped = false;
    let mut quotes = 0;
    let mut depth = 0;
    // 1 right after the `[`, 2 after a `^` there, and 3 once a member has been read.
    let mut class_at = 0;
    for letter in pattern.chars() {
        if wide && letter.len_utf8() > 1 {
            if escaped {
                out.push('\\');
                out.push(letter);
                escaped = false;
            } else if Some(letter) == escape {
                escaped = true;
            } else {
                out.push(letter);
            }
            continue;
        }
        if escaped {
            if letter == '"' && depth < 1 {
                match quotes {
                    0 => out.push_str("){1,1}?("),
                    1 => out.push_str("){1,1}(?:"),
                    _ => {
                        let message = "SQL regular expression may not contain more than two \
                                       escape-double-quote separators";
                        let error = Error::invalid_input(message)
                            .state(SqlState::INVALID_USE_OF_ESCAPE_CHARACTER);
                        return Err(error.unplaced());
                    }
                }
                quotes += 1;
            } else {
                out.push('\\');
                out.push(letter);
                class_at = 3;
            }
            escaped = false;
        } else if Some(letter) == escape {
            escaped = true;
        } else if depth > 0 {
            if letter == '\\' {
                out.push('\\');
            }
            out.push(letter);
            match letter {
                ']' if class_at > 2 => depth -= 1,
                '[' => {
                    depth += 1;
                    class_at = 3;
                }
                '^' => class_at += 1,
                _ => class_at = 3,
            }
        } else {
            match letter {
                '[' => {
                    depth = 1;
                    class_at = 1;
                    out.push('[');
                }
                '%' => out.push_str(".*"),
                '_' => out.push('.'),
                '(' => out.push_str("(?:"),
                '\\' | '.' | '^' | '$' => {
                    out.push('\\');
                    out.push(letter);
                }
                _ => out.push(letter),
            }
        }
    }
    out.push_str(")$");
    Ok(out)
}

/// Whether `name` is one of the calls of this module.
pub(crate) fn is_pg_regexp(name: &str) -> bool {
    Function::of_kernel(name).is_some()
}

/// A compiled pattern with its flags, and the text it was compiled from.
#[derive(Debug)]
pub(crate) struct Call {
    pattern: String,
    letters: String,
    regex: Regex,
    global: bool,
}

/// The numbers of a call, checked as PostgreSQL checks them.
struct Numbers {
    /// How many characters the search skips.
    skip: usize,
    n: usize,
    end: bool,
    subexpr: usize,
}

impl Numbers {
    /// The numbers of one row, or the error for the first one that is out of range. They are
    /// checked before the flags and the pattern, as in PostgreSQL.
    fn read(function: Function, row: &[Value]) -> Result<Self> {
        let number = |parameter: Parameter| {
            let value = function.at(parameter).and_then(|at| row.get(at));
            let value = match value {
                Some(Value::Integer(value)) => *value,
                _ => return Ok(parameter.default_value()),
            };
            let valid = match parameter {
                // An `n` of 0 makes `regexp_replace` replace every match.
                N if function == Function::ReplaceAt => value >= 0,
                Start | N => value > 0,
                EndOption => value == 0 || value == 1,
                _ => value >= 0,
            };
            if !valid {
                let name = match parameter {
                    Start => "start",
                    N => "n",
                    EndOption => "endoption",
                    _ => "subexpr",
                };
                return Err(Error::invalid_input(format!(
                    "invalid value for parameter \"{name}\": {value}"
                ))
                .state(SqlState::INVALID_PARAMETER_VALUE)
                .unplaced());
            }
            Ok(Value::Integer(value))
        };
        let count = |value: Value| match value {
            Value::Integer(value) => usize::try_from(value).unwrap_or(0),
            _ => 0,
        };
        let start = count(number(Start)?);
        let n = count(number(N)?);
        let end = count(number(EndOption)?) == 1;
        let subexpr = count(number(Subexpr)?);
        Ok(Self { skip: start.saturating_sub(1), n, end, subexpr })
    }
}

impl Call {
    /// The call for `pattern` and `letters`, or the error that PostgreSQL gives for them.
    fn new(function: Function, pattern: &str, letters: &str) -> Result<Self> {
        // A string for the fourth argument of `regexp_replace` is the flags, so a number written
        // as a string there is an error that says how to give a start.
        if function == Function::Replace
            && let Some(digit) = letters.chars().next().filter(char::is_ascii_digit)
        {
            let message = format!("invalid regular expression option: \"{digit}\"");
            let hint = "If you meant to use regexp_replace() with a start parameter, cast the \
                        fourth argument to integer explicitly.";
            let error = Error::invalid_input(message).state(SqlState::INVALID_PARAMETER_VALUE);
            return Err(error.hint(hint).unplaced());
        }
        let flags = PgFlags::parse(letters).map_err(Error::unplaced)?;
        // Only the operator, `regexp_matches` and `regexp_replace` take the `g` flag, and the
        // flags are read before the pattern, so this error comes first.
        let global = matches!(
            function,
            Function::Match | Function::Every | Function::Replace | Function::ReplaceAt
        );
        if flags.is_global() && !global {
            let message = format!("{}() does not support the \"global\" option", function.name());
            let error = Error::invalid_input(message).state(SqlState::INVALID_PARAMETER_VALUE);
            let error = match function {
                Function::First => error.hint("Use the regexp_matches function instead."),
                _ => error,
            };
            return Err(error.unplaced());
        }
        let regex = Regex::postgres(pattern, flags).map_err(Error::unplaced)?;
        Ok(Self {
            pattern: pattern.to_owned(),
            letters: letters.to_owned(),
            regex,
            global: flags.is_global(),
        })
    }

    fn holds(&self, pattern: &str, letters: &str) -> bool {
        self.pattern == pattern && self.letters == letters
    }

    /// The answer for one row of arguments, none of which is null.
    fn one(&self, function: Function, row: &[Value], numbers: &Numbers) -> Result<Value> {
        let string = |parameter: Parameter| match function.at(parameter).and_then(|at| row.get(at))
        {
            Some(Value::Varchar(value)) => Ok(value.as_str()),
            _ => Err(Error::internal(format!(
                "{} of something that is not a string",
                function.kernel()
            ))),
        };
        let text = string(Text)?;
        Ok(match function {
            Function::Match | Function::Like => Value::Boolean(self.regex.is_match(text)),
            Function::First => match self.regex.find_at(text, 0) {
                Some(found) => self.pieces(text, &found),
                None => Value::Null,
            },
            Function::Every => {
                let mut rows = Vec::new();
                self.each(text, 0, |found| {
                    rows.push(self.pieces(text, found));
                    self.global
                });
                Value::List { element: Function::First.returns(), values: rows }
            }
            Function::Count => {
                let mut count = 0_usize;
                if let Some(from) = byte_at(text, numbers.skip) {
                    self.each(text, from, |_| {
                        count += 1;
                        true
                    });
                }
                Value::Integer(i32::try_from(count).unwrap_or(i32::MAX))
            }
            Function::Instr => match self.nth(text, numbers) {
                Some((from, to)) => {
                    let at = if numbers.end { to } else { from };
                    let chars = text[..at].chars().count() + 1;
                    Value::Integer(i32::try_from(chars).unwrap_or(i32::MAX))
                }
                None => Value::Integer(0),
            },
            Function::Substr => match self.nth(text, numbers) {
                Some((from, to)) => Value::Varchar(text[from..to].to_owned()),
                None => Value::Null,
            },
            Function::Replace => {
                let n = usize::from(!self.global);
                Value::Varchar(self.replace(text, 0, n, string(Replacement)?))
            }
            Function::ReplaceAt => match byte_at(text, numbers.skip) {
                Some(from) => {
                    Value::Varchar(self.replace(text, from, numbers.n, string(Replacement)?))
                }
                None => Value::Varchar(text.to_owned()),
            },
            Function::SplitArray | Function::SplitTable => {
                let pieces =
                    self.split(text).into_iter().map(|piece| Value::Varchar(piece.to_owned()));
                Value::List { element: LogicalType::Varchar, values: pieces.collect() }
            }
            Function::Substring => {
                let group = usize::from(self.regex.groups() > 0);
                let found = self.regex.find_at(text, 0);
                match found.and_then(|found| found.group(group)) {
                    Some((from, to)) => Value::Varchar(text[from..to].to_owned()),
                    None => Value::Null,
                }
            }
        })
    }

    /// `text` with match `n` from the byte `from` on replaced, or every match for an `n` of 0, as
    /// `replace_text_regexp` in `regexp.c` does.
    fn replace(&self, text: &str, from: usize, n: usize, replacement: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut seen = 0;
        self.each(text, from, |found| {
            seen += 1;
            if seen < n {
                return true;
            }
            out.push_str(&text[copied..found.start()]);
            expand(&mut out, replacement, text, found);
            copied = found.end();
            n == 0
        });
        out.push_str(&text[copied..]);
        out
    }

    /// The pieces of `text` between the matches. An empty match at the start or at the end, or
    /// right after the match before it, does not split, as in `setup_regexp_matches` with
    /// `ignore_degenerate`.
    fn split<'t>(&self, text: &'t str) -> Vec<&'t str> {
        let mut pieces = Vec::new();
        let mut from = 0;
        let mut last_end = 0;
        self.each(text, 0, |found| {
            if found.start() < text.len() && found.end() > last_end {
                pieces.push(&text[from..found.start()]);
                from = found.end();
            }
            last_end = found.end();
            true
        });
        pieces.push(&text[from..]);
        pieces
    }

    /// Calls `keep` with each match from the byte `from` on, while it says to go on. The next
    /// search starts where the last match ended, and one character later when the match was
    /// empty, as `setup_regexp_matches` in `regexp.c` does.
    fn each(&self, text: &str, from: usize, mut keep: impl FnMut(&Captures) -> bool) {
        let mut start = from;
        while let Some(found) = self.regex.find_at(text, start) {
            if !keep(&found) {
                break;
            }
            let (from, to) = (found.start(), found.end());
            start = to;
            if from == to {
                match text[to..].chars().next() {
                    Some(next) => start += next.len_utf8(),
                    None => break,
                }
            }
        }
    }

    /// Where the group `subexpr` of match `n` is, for `regexp_instr` and `regexp_substr`. A pattern
    /// with no groups has the whole match as its group 1, as in PostgreSQL.
    fn nth(&self, text: &str, numbers: &Numbers) -> Option<(usize, usize)> {
        let groups = self.regex.groups();
        if numbers.subexpr > groups.max(1) {
            return None;
        }
        let group = if groups == 0 { 0 } else { numbers.subexpr };
        let from = byte_at(text, numbers.skip)?;
        let mut seen = 0;
        let mut place = None;
        self.each(text, from, |found| {
            seen += 1;
            if seen < numbers.n {
                return true;
            }
            place = found.group(group);
            false
        });
        place
    }

    fn pieces(&self, text: &str, found: &Captures) -> Value {
        let piece = |group: usize| match found.group(group) {
            Some((from, to)) => Value::Varchar(text[from..to].to_owned()),
            None => Value::Null,
        };
        let values = match self.regex.groups() {
            0 => vec![piece(0)],
            groups => (1..=groups).map(piece).collect(),
        };
        Value::List { element: LogicalType::Varchar, values }
    }
}

/// Writes `replacement` for one match. `\1` to `\9` are the groups, and a group that the pattern
/// does not have is empty. `\&` is the whole match and `\\` is one backslash. Any other backslash
/// is kept as it is.
fn expand(out: &mut String, replacement: &str, text: &str, found: &Captures) {
    let mut rest = replacement;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let group = match rest.bytes().next() {
            Some(digit @ b'1'..=b'9') => usize::from(digit - b'0'),
            Some(b'&') => 0,
            Some(b'\\') => {
                out.push('\\');
                rest = &rest[1..];
                continue;
            }
            _ => {
                out.push('\\');
                continue;
            }
        };
        rest = &rest[1..];
        if let Some((from, to)) = found.group(group) {
            out.push_str(&text[from..to]);
        }
    }
    out.push_str(rest);
}

/// The byte that character `chars` of `text` starts at, which is the length for the character
/// after the last, or `None` past that.
fn byte_at(text: &str, chars: usize) -> Option<usize> {
    if chars == 0 {
        return Some(0);
    }
    match text.char_indices().nth(chars) {
        Some((at, _)) => Some(at),
        None => (text.chars().count() == chars).then_some(text.len()),
    }
}

/// The pattern and the flags of a row, with no flags for a function that takes none.
fn pattern_of(function: Function, row: &[Option<&Value>]) -> Option<(String, String)> {
    let Some(Value::Varchar(pattern)) = row.get(1).copied().flatten() else { return None };
    let letters = match function.at(Flags) {
        Some(at) => match row.get(at).copied().flatten() {
            Some(Value::Varchar(letters)) => letters.clone(),
            _ => return None,
        },
        None => String::new(),
    };
    Some((pattern.clone(), letters))
}

/// What a recipe can lift out of a call, given the arguments that were literals. A pattern that
/// does not compile lifts nothing, so its error still comes from the rows.
pub(crate) fn hoist(name: &str, literals: &[Option<Value>]) -> Option<Call> {
    let function = Function::of_kernel(name)?;
    let literals: Vec<Option<&Value>> = literals.iter().map(Option::as_ref).collect();
    let (pattern, letters) = pattern_of(function, &literals)?;
    Call::new(function, &pattern, &letters).ok()
}

/// The answer of a call over one row of values, or `None` when `name` is not one of the calls of
/// this module.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    if name == SIMILAR_ESCAPE {
        return match args {
            [Value::Varchar(pattern), Value::Varchar(escape)] => {
                Ok(Some(Value::Varchar(similar_escape(pattern, escape)?)))
            }
            [_, _] => Ok(Some(Value::Null)),
            _ => Err(Error::internal(format!("{name} takes 2 arguments"))),
        };
    }
    let Some(function) = Function::of_kernel(name) else {
        return Ok(None);
    };
    if args.len() != function.parameters().len() {
        return Err(Error::internal(format!("{name} takes {} arguments", args.len())));
    }
    // The functions are strict.
    if args.iter().any(Value::is_null) {
        return Ok(Some(Value::Null));
    }
    let numbers = Numbers::read(function, args)?;
    let row: Vec<Option<&Value>> = args.iter().map(Some).collect();
    let Some((pattern, letters)) = pattern_of(function, &row) else {
        return Err(Error::internal(format!("{name} of something that is not a string")));
    };
    Call::new(function, &pattern, &letters)?.one(function, args, &numbers).map(Some)
}

/// The answers of a call over vectors, with the pattern compiled again only where it changes.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    prepared: Option<&Call>,
    args: &[V],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let Some(function) = Function::of_kernel(name) else { return Ok(None) };
    let args: Vec<&Vector> = args.iter().map(AsRef::as_ref).collect();
    if args.len() != function.parameters().len() {
        return Ok(None);
    }
    let text = args[0];
    if *text.logical_type() != LogicalType::Varchar {
        return Ok(None);
    }
    let constants: Vec<Option<&Value>> = args.iter().map(|arg| arg.constant_value()).collect();
    let mut held: Option<Call> = None;
    if prepared.is_none()
        && let Some((pattern, letters)) = pattern_of(function, &constants)
    {
        held = Some(Call::new(function, &pattern, &letters)?);
    }
    // The operator over one pattern for every row is the loop that a `WHERE` runs, so it writes
    // the answers in place.
    if matches!(function, Function::Match | Function::Like)
        && let Some(call) = prepared.or(held.as_ref())
    {
        let mut out = vec![false; rows];
        let validity = over_valid(rows, nulls_of(text), |at| {
            out[at] = call.regex.is_match(text.try_text_at(at)?.unwrap_or_default());
            Ok(())
        })?;
        return finish(returns, Data::Bool(out.into()), validity);
    }
    let mut values = Vec::with_capacity(rows);
    let mut row = Vec::with_capacity(args.len());
    // row at a time: each row searches its own text, and gives a list or a string whose length
    // only the search knows.
    for at in 0..rows {
        row.clear();
        for arg in &args {
            row.push(arg.try_value_at(at)?);
        }
        if row.iter().any(Value::is_null) {
            values.push(Value::Null);
            continue;
        }
        let numbers = Numbers::read(function, &row)?;
        let call = match prepared {
            Some(call) => call,
            None => {
                let given: Vec<Option<&Value>> = row.iter().map(Some).collect();
                let Some((pattern, letters)) = pattern_of(function, &given) else {
                    return Err(Error::internal(format!(
                        "{name} of something that is not a string"
                    )));
                };
                if !held.as_ref().is_some_and(|call| call.holds(&pattern, &letters)) {
                    held = Some(Call::new(function, &pattern, &letters)?);
                }
                held.as_ref().ok_or_else(|| Error::internal("no pattern"))?
            }
        };
        values.push(call.one(function, &row, &numbers)?);
    }
    Ok(Some(Vector::from_values(returns.clone(), &values)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::Varchar(value.to_owned())
    }

    /// The answer of `function` over the arguments, with the defaults for the ones left out.
    fn answer(function: Function, given: &[Value]) -> Result<Value> {
        let parameters = function.parameters();
        let mut args = given.to_vec();
        args.extend(parameters[given.len()..].iter().map(|parameter| parameter.default_value()));
        Ok(call(function.kernel(), &args)?.expect("one of these"))
    }

    fn list(values: &[Option<&str>]) -> Value {
        let values = values.iter().map(|value| value.map_or(Value::Null, text)).collect();
        Value::List { element: LogicalType::Varchar, values }
    }

    fn rows(value: Value) -> Vec<Value> {
        let Value::List { values, .. } = value else { panic!("a list: {value:?}") };
        values
    }

    #[test]
    fn regexp_matches_with_g_steps_past_an_empty_match() {
        let every = |string, pattern, flags| {
            rows(answer(Function::Every, &[text(string), text(pattern), text(flags)]).unwrap())
        };
        assert_eq!(every("abc", "x*", "g"), vec![list(&[Some("")]); 4]);
        assert_eq!(every("a1b22", "(\\d+)", "g"), vec![list(&[Some("1")]), list(&[Some("22")])]);
        assert!(every("ab", "x", "").is_empty());
    }

    #[test]
    fn regexp_match_gives_the_groups_or_null() {
        let first = |string, pattern, flags| {
            answer(Function::First, &[text(string), text(pattern), text(flags)])
        };
        assert_eq!(first("abc", "b(c)(x)?", "").unwrap(), list(&[Some("c"), None]));
        assert_eq!(first("abc", "B", "i").unwrap(), list(&[Some("b")]));
        assert_eq!(first("abc", "x", "").unwrap(), Value::Null);
        let error = first("abc", "b", "g").unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::INVALID_PARAMETER_VALUE));
        let args = [Value::Null, text("b"), text("")];
        assert_eq!(call(Function::First.kernel(), &args).unwrap(), Some(Value::Null));
    }

    /// The answers of the PostgreSQL 19 oracle.
    #[test]
    fn the_positions_count_characters_from_the_start() {
        let int = Value::Integer;
        let count = |given: &[Value]| answer(Function::Count, given).unwrap();
        assert_eq!(count(&[text("ABCABCAXYaxy"), text("A.")]), int(3));
        assert_eq!(count(&[text("ABCABCAXYaxy"), text("A."), int(1), text("i")]), int(4));
        assert_eq!(count(&[text("abc"), text(""), int(4)]), int(1));
        assert_eq!(count(&[text("abc"), text("b"), int(9)]), int(0));
        let instr = |given: &[Value]| answer(Function::Instr, given).unwrap();
        let abc = || text("abcabcabc");
        assert_eq!(instr(&[abc(), text("c"), int(1), int(2)]), int(6));
        assert_eq!(instr(&[abc(), text("c"), int(1), int(2), int(1)]), int(7));
        assert_eq!(instr(&[text("éaé"), text("é"), int(2)]), int(3));
        let groups = || text("(b)(c)");
        assert_eq!(instr(&[abc(), groups(), int(1), int(1), int(0), text(""), int(2)]), int(3));
        assert_eq!(instr(&[abc(), groups(), int(1), int(1), int(0), text(""), int(3)]), int(0));
        assert_eq!(instr(&[abc(), text("c"), int(1), int(1), int(0), text(""), int(1)]), int(3));
        let substr = |given: &[Value]| answer(Function::Substr, given).unwrap();
        assert_eq!(substr(&[abc(), text("b."), int(3), int(2)]), text("bc"));
        assert_eq!(substr(&[abc(), text("x")]), Value::Null);
        let error = answer(Function::Instr, &[abc(), text("c"), int(0)]).unwrap_err();
        assert_eq!(error.message(), "invalid value for parameter \"start\": 0");
        let error = answer(Function::Count, &[abc(), text("c"), int(1), text("g")]).unwrap_err();
        assert_eq!(error.message(), "regexp_count() does not support the \"global\" option");
    }

    /// The answers of the PostgreSQL 19 oracle.
    #[test]
    fn regexp_replace_and_the_splits_skip_as_postgresql_does() {
        let int = Value::Integer;
        let replace = |given: &[Value]| answer(Function::Replace, given).unwrap();
        assert_eq!(replace(&[text("abc"), text("x*"), text("-"), text("g")]), text("-a-b-c-"));
        let groups = [text("abcabc"), text("(b)(c)"), text("[\\2\\1\\&\\\\\\3\\x]"), text("g")];
        assert_eq!(replace(&groups), text("a[cbbc\\\\x]a[cbbc\\\\x]"));
        assert_eq!(replace(&[text("abc"), text("b"), text("X\\")]), text("aX\\c"));
        let at = |given: &[Value]| answer(Function::ReplaceAt, given).unwrap();
        let abc = || text("abcabcabc");
        assert_eq!(at(&[abc(), text("b"), text("X"), int(1), int(2)]), text("abcaXcabc"));
        assert_eq!(at(&[abc(), text("b"), text("X"), int(1), int(0)]), text("aXcaXcaXc"));
        assert_eq!(at(&[abc(), text("b"), text("X"), int(10)]), abc());
        let error =
            answer(Function::Replace, &[abc(), text("b"), text("X"), text("2")]).unwrap_err();
        assert_eq!(error.message(), "invalid regular expression option: \"2\"");
        let split = |string, pattern| {
            rows(answer(Function::SplitArray, &[text(string), text(pattern)]).unwrap())
        };
        assert_eq!(split("abc", "x*"), vec![text("a"), text("b"), text("c")]);
        assert_eq!(split(",a,", ","), vec![text(""), text("a"), text("")]);
        assert_eq!(split("", ","), vec![text("")]);
    }

    /// The translations of the PostgreSQL 19 oracle. The brackets end at the first `]` that is not
    /// the first member of the class.
    #[test]
    fn similar_to_escape_writes_what_postgresql_writes() {
        let escape = |pattern, escape| similar_escape(pattern, escape).unwrap();
        assert_eq!(escape("a%b_c", "\\"), "^(?:a.*b.c)$");
        assert_eq!(escape("(ab)*c", "\\"), "^(?:(?:ab)*c)$");
        assert_eq!(escape("a.b^c$d\\e", "\\"), "^(?:a\\.b\\^c\\$d\\e)$");
        assert_eq!(escape("[^]a]x", "\\"), "^(?:[^]a]x)$");
        assert_eq!(escape("[%_]%", "\\"), "^(?:[%_].*)$");
        assert_eq!(escape("[\\\"]\"", "\\"), "^(?:[\\\"]\")$");
        assert_eq!(escape("a#\"b#\"c", "#"), "^(?:a){1,1}?(b){1,1}(?:c)$");
        assert_eq!(escape("a\\%", ""), "^(?:a\\\\.*)$");
        assert_eq!(escape("aé%", "é"), "^(?:a\\%)$");
        let error = similar_escape("a", "ab").unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::INVALID_ESCAPE_SEQUENCE));
        let error = similar_escape("a#\"b#\"c#\"", "#").unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::INVALID_USE_OF_ESCAPE_CHARACTER));
    }
}
