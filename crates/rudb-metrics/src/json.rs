//! Writing JSON, because there is nothing in the workspace that does it yet.
//!
//! The dependency rule leaves no alternative: there is no serde here and `rudb-json` is a stub that
//! reads nothing and writes nothing. What this needs to write is one object of known shape, so it
//! is a string builder with the two things a hand written writer gets wrong made impossible.
//!
//! The first is an unbalanced brace, which is why a nested value is a closure rather than a pair of
//! open and close calls. The second is a comma in the wrong place, which is why the caller never
//! writes one: the writer knows whether the value it is about to write is the first in its object
//! or its array, and that is the whole of the bookkeeping.
//!
//! The output is indented rather than compact. It is read by people as often as by programs, a
//! metrics document is a few kilobytes, and a diff of two of them is only useful line by line.

/// A JSON document being built.
#[derive(Debug)]
pub(crate) struct Writer {
    out: String,
    depth: usize,
    /// Whether the next value is the first one in the object or array being written, which is the
    /// only thing that decides whether it needs a comma in front of it.
    first: bool,
}

impl Writer {
    /// Builds one object and returns it with the newline that ends the file.
    pub(crate) fn document(fill: impl FnOnce(&mut Self)) -> String {
        let mut writer = Self { out: String::new(), depth: 0, first: true };
        writer.object(fill);
        writer.out.push('\n');
        writer.out
    }

    /// An object, whose keys are whatever `fill` writes.
    pub(crate) fn object(&mut self, fill: impl FnOnce(&mut Self)) {
        self.nest('{', fill, '}');
    }

    /// An array, whose elements are whatever `fill` writes.
    pub(crate) fn array(&mut self, fill: impl FnOnce(&mut Self)) {
        self.nest('[', fill, ']');
    }

    fn nest(&mut self, open: char, fill: impl FnOnce(&mut Self), close: char) {
        self.out.push(open);
        let outer = std::mem::replace(&mut self.first, true);
        self.depth += 1;
        fill(self);
        self.depth -= 1;
        // Nothing was written, so the pair goes on one line. `{}` beats a brace with a blank line
        // in it, and this is the common case for an empty list.
        let empty = self.first;
        self.first = outer;
        if !empty {
            self.line();
        }
        self.out.push(close);
    }

    /// A key, followed by whatever the caller writes next.
    pub(crate) fn key(&mut self, name: &str) {
        self.comma();
        self.string(name);
        self.out.push_str(": ");
    }

    /// The start of the next element of an array, followed by whatever the caller writes next.
    pub(crate) fn item(&mut self) {
        self.comma();
    }

    /// A number under a name.
    pub(crate) fn count(&mut self, name: &str, value: u64) {
        self.key(name);
        self.number(value);
    }

    /// A number under a name, or null when there is no answer rather than a zero.
    pub(crate) fn maybe_count(&mut self, name: &str, value: Option<u64>) {
        self.key(name);
        match value {
            Some(value) => self.number(value),
            None => self.out.push_str("null"),
        }
    }

    /// A string under a name.
    pub(crate) fn words(&mut self, name: &str, value: &str) {
        self.key(name);
        self.text(value);
    }

    /// A string under a name, or null. An empty string and a missing one are different answers and
    /// a document that wrote both as `""` would lose the difference.
    pub(crate) fn maybe_words(&mut self, name: &str, value: Option<&str>) {
        self.key(name);
        match value {
            Some(value) => self.text(value),
            None => self.out.push_str("null"),
        }
    }

    /// A boolean under a name.
    pub(crate) fn flag(&mut self, name: &str, value: bool) {
        self.key(name);
        self.out.push_str(if value { "true" } else { "false" });
    }

    /// A bare number, for an array element.
    pub(crate) fn number(&mut self, value: u64) {
        self.out.push_str(&value.to_string());
    }

    /// A bare string, for an array element.
    pub(crate) fn text(&mut self, value: &str) {
        self.string(value);
    }

    fn comma(&mut self) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        self.line();
    }

    fn line(&mut self) {
        self.out.push('\n');
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
    }

    /// A quoted string with the escapes JSON requires.
    ///
    /// A SQL statement is one of the values written here, so this is not a formality: a query with
    /// a quoted literal or a newline in it goes through this, and an unescaped one would produce a
    /// document nothing can parse.
    fn string(&mut self, value: &str) {
        self.out.push('"');
        for character in value.chars() {
            match character {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                '\u{8}' => self.out.push_str("\\b"),
                '\u{c}' => self.out.push_str("\\f"),
                // Everything else below a space has no short escape and has to be spelled out.
                control if (control as u32) < 0x20 => {
                    self.out.push_str(&format!("\\u{:04x}", control as u32));
                }
                other => self.out.push(other),
            }
        }
        self.out.push('"');
    }
}

#[cfg(test)]
mod tests {
    use super::Writer;

    #[test]
    fn an_empty_object_is_two_characters() {
        assert_eq!(Writer::document(|_| {}), "{}\n");
    }

    #[test]
    fn keys_are_separated_and_indented() {
        let out = Writer::document(|writer| {
            writer.count("schema", 1);
            writer.words("state", "succeeded");
            writer.flag("reference", true);
            writer.maybe_count("memory_limit", None);
        });
        assert_eq!(
            out,
            "{\n  \"schema\": 1,\n  \"state\": \"succeeded\",\n  \"reference\": true,\n  \"memory_limit\": null\n}\n"
        );
    }

    #[test]
    fn an_array_of_objects_nests_one_level_at_a_time() {
        let out = Writer::document(|writer| {
            writer.key("operators");
            writer.array(|writer| {
                for id in 0..2 {
                    writer.item();
                    writer.object(|writer| writer.count("id", id));
                }
            });
            writer.key("depends_on");
            writer.array(|writer| {
                writer.item();
                writer.number(7);
            });
            writer.key("warnings");
            writer.array(|_| {});
        });
        let expected = "{\n  \"operators\": [\n    {\n      \"id\": 0\n    },\n    {\n      \"id\": 1\n    }\n  ],\n  \"depends_on\": [\n    7\n  ],\n  \"warnings\": []\n}\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn a_query_with_quotes_and_control_characters_comes_back_out_escaped() {
        let out = Writer::document(|writer| {
            writer.words("sql", "select 'a\\b'\nfrom \"t\"\tlimit 1\u{1}");
        });
        assert_eq!(out, "{\n  \"sql\": \"select 'a\\\\b'\\nfrom \\\"t\\\"\\tlimit 1\\u0001\"\n}\n");
    }
}
