//! The path functions: `parse_path`, `parse_dirname`, `parse_dirpath` and `parse_filename`.
//!
//! They are a port of `parse_path.cpp` on `v2.0.0-dev84237` and not of what a path library would
//! say, because the pin has opinions of its own. A separator named `system` or `forward_slash` is
//! `/`, one named `backslash` is `\`, and any other spelling at all is both of them, so
//! `parse_path('a/b\c', 'BACKSLASH')` splits on either and is `[a, b, c]`. A leading separator is an
//! element of its own and every other run of them is skipped, so `parse_path('//a//b/')` is
//! `[/, a, b]`.
//!
//! The optional arguments are read the way the pin reads them. It looks at the first row of the
//! separator and of `trim_extension`, and when that row is null the argument is not there and the
//! default is used for every row. When it is not null the whole column is used, and a null further
//! down it is a null answer for that row, except in `parse_path`, which takes the separator of the
//! first row and uses it on every row. Only the vectorized path here can see which row is first, so
//! the row at a time path treats every null option as the default, which is what the pin does for a
//! constant.
//!
//! `path_join` is the other half. It is the pin's `Path` class and not string concatenation: every
//! argument is parsed into a scheme, an authority, a root and the segments under it, `.` and `..`
//! are resolved, and an absolute path joins onto another only when it is that path or goes deeper
//! into it, so `path_join('/a', '/b')` is refused where `path_join('/a', '/a/b')` is `/a/b`. Only
//! the parts of that class that are not Windows are here, since that is the pin that was measured.
//!
//! Everything works on bytes. The separators and the `.` of an extension are all ASCII, so a cut at
//! one of them is always on a character boundary.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, StringColumn, Validity, Vector};

/// Whether a name is one of the functions here.
pub(crate) fn is_path(name: &str) -> bool {
    matches!(
        name,
        "parse_path" | "parse_dirname" | "parse_dirpath" | "parse_filename" | "path_join"
    )
}

/// The row at a time answer, which comes before the null rule because a null option is the
/// default rather than a null answer.
pub(crate) fn before_nulls(name: &str, args: &[Value]) -> Option<Result<Value>> {
    if !is_path(name) {
        return None;
    }
    // `path_join` is null in null out like any other function.
    if name == "path_join" {
        if args.iter().any(Value::is_null) {
            return Some(Ok(Value::Null));
        }
        let mut texts = Vec::with_capacity(args.len());
        for arg in args {
            match arg {
                Value::Varchar(text) => texts.push(text.as_str()),
                other => {
                    let error = format!("path_join of a {}", other.logical_type());
                    return Some(Err(Error::internal(error)));
                }
            }
        }
        return Some(joined(&texts).map(Value::Varchar));
    }
    let Some(first) = args.first() else {
        return Some(Err(Error::internal(format!("{name} with no path"))));
    };
    let path = match first {
        Value::Null => return Some(Ok(Value::Null)),
        Value::Varchar(path) => path.as_str(),
        other => {
            return Some(Err(Error::internal(format!("{name} of a {}", other.logical_type()))));
        }
    };
    let options = match options(name, &args[1..]) {
        Ok(options) => options,
        Err(error) => return Some(Err(error)),
    };
    let separators = separators(options.separator.unwrap_or("default"));
    Some(Ok(match name {
        "parse_path" => split_value(path, separators),
        _ => Value::Varchar(trimmed(name, path, separators, options.trim.unwrap_or(false)).into()),
    }))
}

/// The vectorized path, which reads the first row of each option the way the pin does.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    rows: usize,
) -> Result<Option<Vector>> {
    if !is_path(name) {
        return Ok(None);
    }
    let Some((path, rest)) = args.split_first() else {
        return Ok(None);
    };
    let path = path.as_ref();
    if name == "path_join" {
        return join_column(args, rows).map(Some);
    }
    let (separator, trim) = columns(name, rest);
    // An option whose first row is null is an option that was not given.
    let separator = separator.filter(|column| column.try_value_at(0).is_ok_and(|v| !v.is_null()));
    let trim = trim.filter(|column| column.try_value_at(0).is_ok_and(|v| !v.is_null()));
    if name == "parse_path" {
        let spelled = match separator {
            Some(column) => column.try_text_at(0)?.unwrap_or("default").to_string(),
            None => "default".to_string(),
        };
        return split_column(path, separators(&spelled), rows).map(Some);
    }
    let mut out = StringColumn::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    for index in 0..rows {
        let text = path.try_text_at(index)?;
        let spelled = match separator {
            Some(column) => column.try_text_at(index)?,
            None => Some("default"),
        };
        let trimming = match trim {
            Some(column) => column.try_value_at(index)?.as_bool(),
            None => Some(false),
        };
        match (text, spelled, trimming) {
            (Some(text), Some(spelled), Some(trimming)) => {
                out.push(trimmed(name, text, separators(spelled), trimming));
                valid.push(true);
            }
            _ => {
                out.push("");
                valid.push(false);
            }
        }
    }
    let answer = Vector::flat(LogicalType::Varchar, Data::Varlen(out))?;
    Ok(Some(answer.with_validity(Validity::from_run(&valid))))
}

/// The separator and the `trim_extension` columns of a call, by where its overload puts them.
fn columns<'a, V: AsRef<Vector>>(
    name: &str,
    rest: &'a [V],
) -> (Option<&'a Vector>, Option<&'a Vector>) {
    match rest {
        [] => (None, None),
        [second] => {
            let second = second.as_ref();
            if name == "parse_filename" && *second.logical_type() == LogicalType::Boolean {
                (None, Some(second))
            } else {
                (Some(second), None)
            }
        }
        [trim, separator, ..] => (Some(separator.as_ref()), Some(trim.as_ref())),
    }
}

/// The options of one row, where a null is an option that was not given.
struct Options<'a> {
    separator: Option<&'a str>,
    trim: Option<bool>,
}

fn options<'a>(name: &str, rest: &'a [Value]) -> Result<Options<'a>> {
    let text = |value: &'a Value| match value {
        Value::Varchar(text) => Ok(Some(text.as_str())),
        Value::Null => Ok(None),
        other => Err(Error::internal(format!("a path separator of {}", other.logical_type()))),
    };
    Ok(match rest {
        [] => Options { separator: None, trim: None },
        [Value::Boolean(trim)] if name == "parse_filename" => {
            Options { separator: None, trim: Some(*trim) }
        }
        [second] => Options { separator: text(second)?, trim: None },
        [trim, separator, ..] => Options { separator: text(separator)?, trim: trim.as_bool() },
    })
}

/// The bytes a separator option names. Anything the pin does not know is both slashes.
fn separators(option: &str) -> &'static [u8] {
    match option {
        "system" | "forward_slash" => b"/",
        "backslash" => b"\\",
        _ => b"/\\",
    }
}

fn first_of(text: &[u8], separators: &[u8]) -> Option<usize> {
    text.iter().position(|byte| separators.contains(byte))
}

fn last_of(text: &[u8], separators: &[u8]) -> Option<usize> {
    text.iter().rposition(|byte| separators.contains(byte))
}

/// `parse_dirname`, `parse_dirpath` or `parse_filename` of one path.
fn trimmed<'t>(name: &str, text: &'t str, separators: &[u8], trim: bool) -> &'t str {
    let bytes = text.as_bytes();
    match name {
        // Everything before the first separator, or the separator itself when the path starts
        // with one.
        "parse_dirname" => {
            let end = match first_of(bytes, separators) {
                Some(0) => 1,
                Some(at) => at,
                None => 0,
            };
            &text[..end]
        }
        // Everything before the last separator, with the root kept when it is the whole path.
        "parse_dirpath" => {
            let end = match last_of(bytes, separators) {
                Some(0) if bytes.len() == 1 => 1,
                Some(at) => at,
                None => 0,
            };
            &text[..end]
        }
        // Everything after the last separator, less the extension when it is asked to go. The last
        // `.` is looked for in the whole path, so a dot in a directory name is not an extension.
        _ => {
            let begin = last_of(bytes, separators).map_or(0, |at| at + 1);
            let mut end = bytes.len();
            if trim && let Some(dot) = bytes.iter().rposition(|&byte| byte == b'.') {
                if begin <= dot {
                    end = dot;
                }
            }
            &text[begin..end]
        }
    }
}

/// The pieces `parse_path` cuts one path into, handed to `emit` in order.
fn split<'t>(text: &'t str, separators: &[u8], mut emit: impl FnMut(&'t str)) {
    let mut rest = text;
    let mut emitted = false;
    while let Some(at) = first_of(rest.as_bytes(), separators) {
        if at == 0 {
            // A separator at the very start is the root and is an element. One anywhere else is
            // the second of two in a row and is skipped.
            if !emitted {
                emit(&rest[..1]);
                emitted = true;
                if rest.len() == 1 {
                    return;
                }
            }
        } else {
            emit(&rest[..at]);
            emitted = true;
        }
        rest = &rest[at + 1..];
    }
    if !rest.is_empty() {
        emit(rest);
    }
}

fn split_value(text: &str, separators: &[u8]) -> Value {
    let mut values = Vec::new();
    split(text, separators, |piece| values.push(Value::Varchar(piece.to_string())));
    Value::List { element: LogicalType::Varchar, values }
}

/// `parse_path` of a column, built as one child column of every piece in the vector.
fn split_column(path: &Vector, separators: &[u8], rows: usize) -> Result<Vector> {
    let mut entries = Vec::with_capacity(rows);
    let mut child = StringColumn::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    for index in 0..rows {
        let begin = child.len();
        let text = path.try_text_at(index)?;
        if let Some(text) = text {
            split(text, separators, |piece| {
                child.push(piece);
            });
        }
        valid.push(text.is_some());
        let offset = u32::try_from(begin).map_err(|_| Error::internal("a list over 4GB"))?;
        let len =
            u32::try_from(child.len() - begin).map_err(|_| Error::internal("a list over 4GB"))?;
        entries.push((offset, len));
    }
    let child = Vector::flat(LogicalType::Varchar, Data::Varlen(child))?;
    Ok(Vector::list(entries, child)?.with_validity(Validity::from_run(&valid)))
}

/// `path_join` of a vector, a row at a time since every row is parsed on its own anyway.
fn join_column<V: AsRef<Vector>>(args: &[V], rows: usize) -> Result<Vector> {
    let mut out = StringColumn::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    let mut texts = Vec::with_capacity(args.len());
    for index in 0..rows {
        texts.clear();
        for arg in args {
            match arg.as_ref().try_text_at(index)? {
                Some(text) => texts.push(text),
                None => break,
            }
        }
        if texts.len() == args.len() {
            out.push(&joined(&texts)?);
            valid.push(true);
        } else {
            out.push("");
            valid.push(false);
        }
    }
    let answer = Vector::flat(LogicalType::Varchar, Data::Varlen(out))?;
    Ok(answer.with_validity(Validity::from_run(&valid)))
}

/// The paths joined one onto the next, left to right.
fn joined(texts: &[&str]) -> Result<String> {
    let Some((first, rest)) = texts.split_first() else {
        return Err(Error::internal("path_join of nothing"));
    };
    let mut path = Path::parse(first)?;
    for text in rest {
        path = path.join(Path::parse(text)?)?;
    }
    Ok(path.to_string())
}

/// A path as the pin's `Path` class reads it, where the root is `/` or nothing and the separator
/// written back is always `/`.
#[derive(Debug, Default)]
struct Path {
    /// `s3://` or `file:` and the like, in lower case.
    scheme: String,
    /// The bucket or the host after a scheme with `://` in it.
    authority: String,
    /// Whether there is a root, which is also whether the path is absolute.
    rooted: bool,
    segments: Vec<String>,
    /// Whether the text ended in a separator, which is written back after the last segment.
    trailing: bool,
}

fn is_separator(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\')
}

impl Path {
    fn parse(raw: &str) -> Result<Self> {
        let mut path = Self::default();
        let first_slash = raw.find('/');
        let scheme_at = raw.find("://");
        let offset = if raw.starts_with("file:/") {
            path.file_scheme(raw)?
        } else if let Some(at) = scheme_at
            && at > 1
            && first_slash.is_none_or(|slash| at < slash)
        {
            path.uri(raw)
        } else {
            path.root(raw, 0)
        };
        // A `..` pops the segment before it unless there is none or it is another `..`, and one
        // that would climb above the root is dropped.
        for segment in raw[offset..].split(['/', '\\']) {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." && path.segments.last().is_some_and(|last| last != "..") {
                path.segments.pop();
            } else if !(segment == ".." && path.rooted && path.segments.is_empty()) {
                path.segments.push(segment.to_string());
            }
        }
        path.trailing = raw.bytes().last().is_some_and(is_separator);
        Ok(path)
    }

    /// Reads a root at `at`, and answers where the segments start.
    fn root(&mut self, raw: &str, at: usize) -> usize {
        if raw.as_bytes().get(at).copied().is_some_and(is_separator) {
            self.rooted = true;
            return at + 1;
        }
        at
    }

    /// `proto://authority/path`, which is always absolute.
    fn uri(&mut self, raw: &str) -> usize {
        self.rooted = true;
        let begin = raw.find("://").map_or(0, |at| at + 3);
        self.scheme = raw[..begin].to_ascii_lowercase();
        let end = raw[begin..].find('/').map(|at| at + begin);
        self.authority = raw[begin..end.unwrap_or(raw.len())].to_string();
        end.map_or(raw.len(), |end| end + 1)
    }

    /// `file:/path`, `file:///path` and `file://localhost/path`, and no other host.
    fn file_scheme(&mut self, raw: &str) -> Result<usize> {
        let bytes = raw.as_bytes();
        self.rooted = true;
        let begin = if bytes.get(6..8) == Some(b"//") {
            self.scheme = "file://".to_string();
            8
        } else if bytes.get(6) == Some(&b'/') {
            let begin = self.uri(raw);
            if !self.authority.eq_ignore_ascii_case("localhost") {
                return Err(Error::invalid_input(format!(
                    "Path: file:// scheme only supports localhost authority, got: {}",
                    self.authority
                )));
            }
            begin
        } else {
            self.scheme = "file:".to_string();
            6
        };
        Ok(self.root(raw, begin))
    }

    fn join(mut self, other: Self) -> Result<Self> {
        if !other.rooted {
            let mut segments: Vec<String> = Vec::with_capacity(self.segments.len());
            for segment in self.segments.drain(..).chain(other.segments) {
                if segment == ".." && segments.last().is_some_and(|last| last != "..") {
                    segments.pop();
                } else {
                    segments.push(segment);
                }
            }
            if self.rooted {
                let climbing = segments.iter().take_while(|segment| *segment == "..").count();
                segments.drain(..climbing);
            }
            self.segments = segments;
        } else if self.authority == other.authority
            && other.segments.starts_with(&self.segments)
            && self.scheme == other.scheme
            && self.rooted == other.rooted
        {
            self.segments = other.segments;
        } else {
            return Err(Error::invalid_input(format!(
                "Path: cannot join incompatible paths: \"{other}\" onto \"{self}\""
            )));
        }
        self.trailing = other.trailing;
        Ok(self)
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let root = if self.rooted { "/" } else { "" };
        let written = format!("{}{}{root}{}", self.scheme, self.authority, self.segments.join("/"));
        let trailing = if self.trailing && !self.segments.is_empty() { "/" } else { "" };
        if written.is_empty() { f.write_str(".") } else { write!(f, "{written}{trailing}") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces<'t>(text: &'t str, option: &str) -> Vec<&'t str> {
        let mut out = Vec::new();
        split(text, separators(option), |piece| out.push(piece));
        out
    }

    #[test]
    fn a_path_is_cut_where_the_pin_cuts_it() {
        assert_eq!(pieces("/a/b/c", "default"), ["/", "a", "b", "c"]);
        assert_eq!(pieces("a\\b/c", "default"), ["a", "b", "c"]);
        assert_eq!(pieces("//a//b/", "default"), ["/", "a", "b"]);
        assert!(pieces("", "default").is_empty());
        assert_eq!(pieces("/", "default"), ["/"]);
        assert_eq!(pieces("a/b\\c", "system"), ["a", "b\\c"]);
        assert_eq!(pieces("a/b\\c", "backslash"), ["a/b", "c"]);
        assert_eq!(pieces("a/b\\c", "BACKSLASH"), ["a", "b", "c"]);
    }

    #[test]
    fn a_path_is_trimmed_where_the_pin_trims_it() {
        let both = separators("default");
        let cases = [
            ("parse_dirname", "/a/b/c", false, "/"),
            ("parse_dirname", "a/b", false, "a"),
            ("parse_dirname", "/", false, "/"),
            ("parse_dirname", "abc", false, ""),
            ("parse_dirpath", "/a/b/c", false, "/a/b"),
            ("parse_dirpath", "a/b/", false, "a/b"),
            ("parse_dirpath", "/", false, "/"),
            ("parse_dirpath", "/abc", false, ""),
            ("parse_filename", "/a/b/c.txt", false, "c.txt"),
            ("parse_filename", "/a/b/c.txt", true, "c"),
            ("parse_filename", "c.tar.gz", true, "c.tar"),
            ("parse_filename", ".bashrc", true, ""),
            ("parse_filename", "a/b/", true, ""),
            ("parse_filename", "a.b/c", true, "c"),
            ("parse_filename", "é/ö.ü", true, "ö"),
        ];
        for (name, text, trim, expected) in cases {
            assert_eq!(trimmed(name, text, both, trim), expected, "{name}({text}, {trim})");
        }
        assert_eq!(trimmed("parse_dirname", "/a", separators("backslash"), false), "");
        assert_eq!(
            trimmed("parse_filename", "a\\b.c", separators("forward_slash"), false),
            "a\\b.c"
        );
    }

    #[test]
    fn paths_are_joined_the_way_the_pins_path_class_joins_them() {
        let cases: [(&[&str], &str); 24] = [
            (&["a", "b"], "a/b"),
            (&["a"], "a"),
            (&["", "b"], "b"),
            (&["a", ""], "a"),
            (&["a\\", "b"], "a/b"),
            (&["a/./b/../c", "..", "d/"], "a/d/"),
            (&[""], "."),
            (&["/"], "/"),
            (&["/..", ".."], "/"),
            (&["../..", "../x"], "../../../x"),
            (&["/a/b", "/a/b/c"], "/a/b/c"),
            (&["/a/", "/a"], "/a"),
            (&["s3://b", "s3://b/k"], "s3://b/k"),
            (&["//a", "b"], "/a/b"),
            (&["../a", "../../b"], "../../b"),
            (&["/x/..", "y/../.."], "/"),
            (&["S3://Bucket/x", "y"], "s3://Bucket/x/y"),
            (&["file:/a", "b"], "file:/a/b"),
            (&["file:///a", "b/"], "file:///a/b/"),
            (&["file://LOCALHOST/a", "b"], "file://LOCALHOST/a/b"),
            (&["a://b"], "a:/b"),
            (&["ab://c/d/e", "../../.."], "ab://c/"),
            (&["a", "b", "c", "../../.."], "."),
            (&["file:", "x"], "file:/x"),
        ];
        for (texts, expected) in cases {
            assert_eq!(joined(texts).unwrap(), expected, "{texts:?}");
        }
        let refusals: [(&[&str], &str); 4] = [
            (&["/a", "/b"], "cannot join incompatible paths: \"/b\" onto \"/a\""),
            (&["/a/b/c", "/a/b"], "cannot join incompatible paths: \"/a/b\" onto \"/a/b/c\""),
            (
                &["S3://b/k", "s3://b"],
                "cannot join incompatible paths: \"s3://b/\" onto \"s3://b/k\"",
            ),
            (
                &["file://host/a", "b"],
                "file:// scheme only supports localhost authority, got: host",
            ),
        ];
        for (texts, expected) in refusals {
            let said = joined(texts).unwrap_err().to_string();
            assert!(said.contains(expected), "{texts:?}: {said}");
        }
    }
}
