//! `LIKE` and `ILIKE` against a constant pattern.
//!
//! The same shapes and the same walk as the first engine's kernel in `rudb-kernels`, so the two
//! engines answer alike. There is no escape character, because the first engine has none.

use memchr::memmem;

/// A compiled pattern.
#[derive(Debug)]
pub struct Like {
    pattern: Pattern,
    fold: bool,
}

#[derive(Debug)]
enum Pattern {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Contains(Box<memmem::Finder<'static>>),
    Segments { prefix: Vec<u8>, suffix: Vec<u8>, middles: Vec<memmem::Finder<'static>> },
    General(Vec<char>),
}

impl Like {
    /// Compiles `spelling`. `fold` is `ILIKE`.
    #[must_use]
    pub fn new(spelling: &str, fold: bool) -> Like {
        let spelling = if fold { spelling.to_lowercase() } else { spelling.to_owned() };
        Like { pattern: Pattern::compile(&spelling), fold }
    }

    /// Whether `text` matches.
    #[must_use]
    pub fn matches(&self, text: &[u8]) -> bool {
        if self.fold {
            let lower = String::from_utf8_lossy(text).to_lowercase();
            return self.pattern.holds(lower.as_bytes());
        }
        self.pattern.holds(text)
    }
}

impl Pattern {
    fn compile(spelling: &str) -> Pattern {
        let plain = |text: &str| !text.contains('%') && !text.contains('_');
        if plain(spelling) {
            return Pattern::Exact(spelling.as_bytes().to_vec());
        }
        if let Some(inner) = spelling.strip_prefix('%').and_then(|rest| rest.strip_suffix('%'))
            && plain(inner)
        {
            return Pattern::Contains(Box::new(memmem::Finder::new(inner).into_owned()));
        }
        if let Some(rest) = spelling.strip_prefix('%')
            && plain(rest)
        {
            return Pattern::Suffix(rest.as_bytes().to_vec());
        }
        if let Some(head) = spelling.strip_suffix('%')
            && plain(head)
        {
            return Pattern::Prefix(head.as_bytes().to_vec());
        }
        if !spelling.contains('_') {
            let mut pieces: Vec<&str> = spelling.split('%').collect();
            if pieces.len() >= 2 {
                let suffix = pieces.pop().unwrap_or_default().as_bytes().to_vec();
                let prefix = pieces.remove(0).as_bytes().to_vec();
                let middles = pieces
                    .into_iter()
                    .filter(|piece| !piece.is_empty())
                    .map(|piece| memmem::Finder::new(piece).into_owned())
                    .collect();
                return Pattern::Segments { prefix, suffix, middles };
            }
        }
        Pattern::General(spelling.chars().collect())
    }

    fn holds(&self, text: &[u8]) -> bool {
        match self {
            Pattern::Exact(p) => text == &p[..],
            Pattern::Prefix(p) => text.starts_with(p),
            Pattern::Suffix(p) => text.ends_with(p),
            Pattern::Contains(f) => f.find(text).is_some(),
            Pattern::Segments { prefix, suffix, middles } => {
                if !text.starts_with(prefix) || !text.ends_with(suffix) {
                    return false;
                }
                let Some(end) = text.len().checked_sub(suffix.len()) else { return false };
                if prefix.len() > end {
                    return false;
                }
                let mut rest = &text[prefix.len()..end];
                for f in middles {
                    let Some(at) = f.find(rest) else { return false };
                    rest = &rest[at + f.needle().len()..];
                }
                true
            }
            Pattern::General(p) => {
                let text: Vec<char> = String::from_utf8_lossy(text).chars().collect();
                walk(&text, p)
            }
        }
    }
}

/// The backtracking walk with one resume point, tried wildcard first so a `%` in the text is not
/// taken for the pattern's.
fn walk(text: &[char], pattern: &[char]) -> bool {
    let (mut at, mut against) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while at < text.len() {
        if against < pattern.len() && pattern[against] == '%' {
            star = Some(against);
            resume = at;
            against += 1;
        } else if against < pattern.len()
            && (pattern[against] == '_' || pattern[against] == text[at])
        {
            at += 1;
            against += 1;
        } else if let Some(s) = star {
            against = s + 1;
            resume += 1;
            at = resume;
        } else {
            return false;
        }
    }
    while against < pattern.len() && pattern[against] == '%' {
        against += 1;
    }
    against == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_answers_like_sql() {
        let cases = [
            ("%google%", "http://www.google.com/", true),
            ("%google%", "http://www.yandex.ru/", false),
            ("%.google.%", "http://www.google.com/", true),
            ("abc", "abc", true),
            ("abc%", "abcdef", true),
            ("%def", "abcdef", true),
            ("a%c%e", "abcde", true),
            ("a%c%e", "ace", true),
            ("ab%bc", "abc", false),
            ("a_c", "abc", true),
            ("a_c", "ac", false),
            ("%a%", "b%a", true),
            ("_é_", "aéb", true),
        ];
        for (p, t, want) in cases {
            assert_eq!(Like::new(p, false).matches(t.as_bytes()), want, "{t} LIKE {p}");
        }
        assert!(Like::new("%GOOGLE%", true).matches(b"www.Google.com"));
    }
}
