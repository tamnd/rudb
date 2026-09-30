//! `GLOB` and the `LIKE ... ESCAPE` family, the two pattern matches that `LIKE` on its own does not
//! cover.
//!
//! Both are ports of the pin's own loops in `like.cpp`, since the corner cases are where the
//! answers differ: an unclosed bracket in a glob matches nothing, a `]` right after the opening
//! bracket is a character and not the end, and a pattern ending in the escape character is an
//! error rather than a literal. `GLOB` works on bytes, the way the pin does, and the escaped
//! `LIKE` works on characters, the way rudb's plain `LIKE` does.

use rudb_common::{Error, Result, Value};

/// `text GLOB pattern`, where `*` is any run, `?` is any one byte, `[...]` is a set with ranges
/// and a leading `!` for its complement, and a backslash makes the byte after it literal.
pub(crate) fn glob(text: &[u8], pattern: &[u8]) -> bool {
    let (mut at, mut against) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while at < text.len() {
        let mut matched = false;
        let mut next = against;
        if let Some(&wanted) = pattern.get(against) {
            match wanted {
                b'*' => {
                    against += 1;
                    while pattern.get(against) == Some(&b'*') {
                        against += 1;
                    }
                    if against == pattern.len() {
                        return true;
                    }
                    star = Some((against, at));
                    continue;
                }
                b'?' => {
                    matched = true;
                    next = against + 1;
                }
                b'[' => {
                    next = against + 1;
                    match bracket(text[at], pattern, &mut next) {
                        Some(found) => matched = found,
                        None => return false,
                    }
                }
                b'\\' => {
                    against += 1;
                    let Some(&literal) = pattern.get(against) else { return false };
                    matched = text[at] == literal;
                    next = against + 1;
                }
                _ => {
                    matched = text[at] == wanted;
                    next = against + 1;
                }
            }
        }
        if matched {
            at += 1;
            against = next;
            continue;
        }
        let Some((resume, from)) = star else { return false };
        star = Some((resume, from + 1));
        at = from + 1;
        against = resume;
    }
    while pattern.get(against) == Some(&b'*') {
        against += 1;
    }
    against == pattern.len()
}

/// Whether `byte` is in the bracket set starting at `at`, which is left just past the closing
/// bracket, or `None` when the set is never closed and the whole pattern matches nothing.
fn bracket(byte: u8, pattern: &[u8], at: &mut usize) -> Option<bool> {
    if *at == pattern.len() {
        return None;
    }
    let invert = pattern[*at] == b'!';
    if invert {
        *at += 1;
    }
    let start = *at;
    let mut found = invert;
    while *at < pattern.len() {
        let first = pattern[*at];
        // A `]` straight after the opening bracket is a member and not the end of the set.
        if first == b']' && *at > start {
            *at += 1;
            return Some(found);
        }
        if *at + 1 == pattern.len() {
            return None;
        }
        let matches = if pattern[*at + 1] == b'-' {
            let &last = pattern.get(*at + 2)?;
            *at += 3;
            (first..=last).contains(&byte)
        } else {
            *at += 1;
            first == byte
        };
        if found == invert && matches {
            found = !invert;
        }
    }
    None
}

/// The escape character of a `LIKE ... ESCAPE`, which is none for an empty string and has to be
/// one character when it is not.
pub(crate) fn escape_char(escape: &Value) -> Result<Option<char>> {
    let text = escape.to_string();
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (None, _) => Ok(None),
        (Some(only), None) if text.len() == 1 => Ok(Some(only)),
        _ => Err(Error::syntax(
            "Invalid escape string. Escape string must be empty or one character.",
        )),
    }
}

/// `text LIKE pattern ESCAPE escape`, where the character after the escape matches itself.
pub(crate) fn like_escaped(text: &[char], pattern: &[char], escape: char) -> Result<bool> {
    const ENDS: &str = "Like pattern must not end with escape character!";
    let (mut against, mut at) = (0usize, 0usize);
    while against < pattern.len() && at < text.len() {
        let wanted = pattern[against];
        if wanted == escape {
            against += 1;
            let Some(&literal) = pattern.get(against) else { return Err(Error::syntax(ENDS)) };
            if literal != text[at] {
                return Ok(false);
            }
            at += 1;
        } else if wanted == '_' {
            at += 1;
        } else if wanted == '%' {
            against += 1;
            while pattern.get(against) == Some(&'%') {
                against += 1;
            }
            if against == pattern.len() {
                return Ok(true);
            }
            for from in at..text.len() {
                if like_escaped(&text[from..], &pattern[against..], escape)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        } else if wanted == text[at] {
            at += 1;
        } else {
            return Ok(false);
        }
        against += 1;
    }
    while against < pattern.len() {
        if pattern[against] == escape {
            if against + 1 == pattern.len() {
                return Err(Error::syntax(ENDS));
            }
            break;
        }
        if pattern[against] != '%' {
            break;
        }
        against += 1;
    }
    Ok(against == pattern.len() && at == text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_the_way_the_pin_does() {
        let cases: [(&str, &str, bool); 12] = [
            ("abc", "a*", true),
            ("abc", "a?c", true),
            ("abc", "b*", false),
            ("a*c", "a[*]c", true),
            ("abc", "[a-c]bc", true),
            ("abc", "[!a]bc", false),
            ("]bc", "[]]bc", true),
            ("abc", "[abc", false),
            ("a*c", "a\\*c", true),
            ("abc", "a\\*c", false),
            ("", "*", true),
            ("abcabd", "*ab?", true),
        ];
        for (text, pattern, expected) in cases {
            assert_eq!(glob(text.as_bytes(), pattern.as_bytes()), expected, "{text} {pattern}");
        }
    }

    #[test]
    fn an_escaped_like_takes_the_character_after_the_escape_literally() {
        let run = |text: &str, pattern: &str, escape: char| {
            let text: Vec<char> = text.chars().collect();
            let pattern: Vec<char> = pattern.chars().collect();
            like_escaped(&text, &pattern, escape)
        };
        assert!(run("a%c", "a\\%c", '\\').unwrap());
        assert!(!run("abc", "a\\%c", '\\').unwrap());
        assert!(!run("a_c", "a$_$", '$').is_ok_and(|held| held));
        assert!(run("a$", "a$$", '$').unwrap());
        assert!(run("abc", "a%", '$').unwrap());
        assert!(run("abc", "a$", '$').is_err());
        assert!(escape_char(&Value::Varchar("xy".into())).is_err());
        assert_eq!(escape_char(&Value::Varchar(String::new())).unwrap(), None);
    }
}
