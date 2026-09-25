//! `string_split` and `string_split_regex` with their aliases, which cut a string at every
//! separator and answer the pieces as a `VARCHAR[]`.
//!
//! The loop is the pin's, and its corners are what make it the pin's. The separator is looked for
//! in what is left of the string rather than in the whole of it, so `^` matches again after every
//! cut and `string_split_regex('a1b2', '^[a-z]')` is `['', 1b2]`. An empty match where a piece
//! starts would cut nothing, so the piece is one character instead, which is how an empty separator
//! splits a string into its characters and `'x*'` does the same. What is left after the last cut is
//! always a piece, even an empty one, so `'a,'` is `[a, '']` and an empty string is `['']`.
//!
//! Both run before the rule that a null argument is a null answer. A null string is still null, but
//! a null separator is a string with nothing to cut at, `string_split('a,b', NULL)` is `['a,b']`,
//! and a null option string is refused with the pin's message.

use std::cell::RefCell;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_regex::{Options, Regex};

/// The answer to one of these functions, or `None` if `name` is not one of them.
pub(crate) fn before_nulls(name: &str, args: &[Value]) -> Option<Result<Value>> {
    let regex = match name {
        "string_split" | "str_split" | "string_to_array" | "split" => false,
        "string_split_regex" | "str_split_regex" | "regexp_split_to_array" => true,
        _ => return None,
    };
    Some(answer(regex, args))
}

fn answer(regex: bool, args: &[Value]) -> Result<Value> {
    let (text, separator, spelling) = match args {
        [text, separator] => (text, separator, ""),
        [_, _, Value::Null] => {
            return Err(Error::invalid_input("Regex options field must not be NULL"));
        }
        [text, separator, Value::Varchar(spelling)] => (text, separator, spelling.as_str()),
        _ => return Err(Error::internal(format!("a split of {args:?}"))),
    };
    let Value::Varchar(text) = text else {
        return Ok(Value::Null);
    };
    let pieces = match separator {
        Value::Varchar(pattern) if regex => {
            let options = Options::parse(spelling)?;
            if options.global {
                return Err(Error::invalid_input(
                    "Option 'g' (global replace) is only valid for regexp_replace",
                ));
            }
            with_compiled(pattern, options, |compiled| {
                cut(text, |rest| compiled.find_at(rest, 0).map(|at| (at.start(), at.end())))
            })?
        }
        Value::Varchar(separator) if separator.is_empty() => cut(text, |_| Some((0, 0))),
        Value::Varchar(separator) => {
            cut(text, |rest| rest.find(separator.as_str()).map(|at| (at, at + separator.len())))
        }
        _ => vec![text.as_str()],
    };
    let values = pieces.into_iter().map(|piece| Value::Varchar(piece.to_string())).collect();
    Ok(Value::List { element: LogicalType::Varchar, values })
}

/// The pieces of `text`, where `find` gives the start and end of the first separator in what it is
/// handed, or `None` when there is none left.
fn cut(text: &str, mut find: impl FnMut(&str) -> Option<(usize, usize)>) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let Some((mut start, end)) = find(rest) else {
            break;
        };
        let mut after = end;
        if start == 0 && end == 0 {
            start = rest.chars().next().map_or(0, char::len_utf8);
            if start == rest.len() {
                break;
            }
            after = start;
        }
        pieces.push(&rest[..start]);
        rest = &rest[after..];
    }
    pieces.push(rest);
    pieces
}

thread_local! {
    /// The last pattern compiled on this thread, since a column is nearly always split by one
    /// constant pattern and compiling it for every row would cost more than the splitting.
    static LAST: RefCell<Option<(String, Options, Regex)>> = const { RefCell::new(None) };
}

fn with_compiled<T>(pattern: &str, options: Options, body: impl FnOnce(&Regex) -> T) -> Result<T> {
    LAST.with(|last| {
        let mut last = last.borrow_mut();
        let fresh = !matches!(&*last, Some((held, set, _)) if held == pattern && *set == options);
        if fresh {
            *last = Some((pattern.to_string(), options, Regex::with_options(pattern, options)?));
        }
        let Some((_, _, compiled)) = last.as_ref() else {
            return Err(Error::internal("a split pattern that was just compiled"));
        };
        Ok(body(compiled))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(name: &str, args: &[Value]) -> Result<Vec<String>> {
        match before_nulls(name, args).expect("a split")? {
            Value::List { values, .. } => Ok(values.iter().map(ToString::to_string).collect()),
            other => panic!("{other:?}"),
        }
    }

    fn text(value: &str) -> Value {
        Value::Varchar(value.to_string())
    }

    #[test]
    fn a_plain_split_keeps_the_empty_pieces_and_an_empty_separator_splits_characters() {
        let plain = |a: &str, b: &str| split("string_split", &[text(a), text(b)]).unwrap();
        assert_eq!(plain("a,b,,c", ","), ["a", "b", "", "c"]);
        assert_eq!(plain("a,", ","), ["a", ""]);
        assert_eq!(plain(",", ","), ["", ""]);
        assert_eq!(plain("", ","), [""]);
        assert_eq!(plain("", ""), [""]);
        assert_eq!(plain("héllo", ""), ["h", "é", "l", "l", "o"]);
        assert_eq!(plain("aXXbXc", "XX"), ["a", "bXc"]);
        assert_eq!(split("split", &[text("a,b"), Value::Null]).unwrap(), ["a,b"]);
        assert_eq!(
            before_nulls("str_split", &[Value::Null, text(",")]).unwrap().unwrap(),
            Value::Null
        );
    }

    #[test]
    fn a_regex_split_looks_again_in_what_is_left() {
        let regex = |a: &str, b: &str| split("string_split_regex", &[text(a), text(b)]).unwrap();
        assert_eq!(regex("a1b22c", "[0-9]+"), ["a", "b", "c"]);
        assert_eq!(regex("a1b2", "^[a-z]"), ["", "1b2"]);
        assert_eq!(regex("aaa", "a"), ["", "", "", ""]);
        assert_eq!(regex("abc", "x*"), ["a", "b", "c"]);
        assert_eq!(regex("abc", "$"), ["abc", ""]);
        assert_eq!(regex("ab", "\\b"), ["a", "b"]);
        let with = |options: Value| {
            split("regexp_split_to_array", &[text("aXbxc"), text("x"), options])
                .map_err(|error| error.to_string())
        };
        assert_eq!(with(text("i")).unwrap(), ["a", "b", "c"]);
        assert!(with(text("g")).unwrap_err().contains("only valid for regexp_replace"));
        assert!(with(text("z")).unwrap_err().contains("Unrecognized Regex option z"));
        assert!(with(Value::Null).unwrap_err().contains("must not be NULL"));
    }
}
