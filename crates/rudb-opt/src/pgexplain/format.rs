//! The writer of the four output formats, a port of PostgreSQL's `explain_format.c`.
//!
//! Each function here has the name of the C function it ports, less the `Explain` prefix, and does
//! the same thing in the same order. The state is the state of `ExplainState` that those functions
//! read: the output text, the indent and the grouping stack. A tool that reads the output of
//! PostgreSQL reads this output, so the spaces, the commas and the line ends are those of the C
//! code and not a simpler form of them.

use rudb_plan::explain::Format;

/// The flags of `ExplainXMLTag`.
const X_OPENING: u8 = 0;
const X_CLOSING: u8 = 1;
const X_CLOSE_IMMEDIATE: u8 = 2;
const X_NOWHITESPACE: u8 = 4;

/// The output of one `EXPLAIN` while it is written.
pub(super) struct Writer {
    pub(super) format: Format,
    pub(super) out: String,
    /// The indent level. Text and XML indent by two spaces a level, and so do JSON and YAML.
    pub(super) indent: usize,
    /// For JSON, whether the group open at each level has a member yet. For YAML, whether the
    /// next line starts a new line. The top of the stack is the last element.
    stack: Vec<u8>,
}

impl Writer {
    pub(super) fn new(format: Format) -> Self {
        Self { format, out: String::new(), indent: 0, stack: Vec::new() }
    }

    pub(super) fn text(&self) -> bool {
        self.format == Format::Text
    }

    /// `ExplainPropertyList`: a label and a list of strings.
    pub(super) fn property_list(&mut self, label: &str, data: &[String]) {
        match self.format {
            Format::Text => {
                self.indent_text();
                self.out.push_str(label);
                self.out.push_str(": ");
                self.out.push_str(&data.join(", "));
                self.out.push('\n');
            }
            Format::Xml => {
                self.xml_tag(label, X_OPENING);
                for item in data {
                    self.spaces(self.indent * 2 + 2);
                    self.out.push_str("<Item>");
                    escape_xml(&mut self.out, item);
                    self.out.push_str("</Item>\n");
                }
                self.xml_tag(label, X_CLOSING);
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                escape_json(&mut self.out, label);
                self.out.push_str(": [");
                for (position, item) in data.iter().enumerate() {
                    if position > 0 {
                        self.out.push_str(", ");
                    }
                    escape_json(&mut self.out, item);
                }
                self.out.push(']');
            }
            Format::Yaml => {
                self.yaml_line_starting();
                self.out.push_str(label);
                self.out.push_str(": ");
                for item in data {
                    self.out.push('\n');
                    self.spaces(self.indent * 2 + 2);
                    self.out.push_str("- ");
                    escape_json(&mut self.out, item);
                }
            }
        }
    }

    /// `ExplainProperty`: a label and one value, with a unit for the text format. A numeric value
    /// has no quotes in JSON and YAML.
    fn property(&mut self, label: &str, unit: Option<&str>, value: &str, numeric: bool) {
        match self.format {
            Format::Text => {
                self.indent_text();
                self.out.push_str(label);
                self.out.push_str(": ");
                self.out.push_str(value);
                if let Some(unit) = unit {
                    self.out.push(' ');
                    self.out.push_str(unit);
                }
                self.out.push('\n');
            }
            Format::Xml => {
                self.spaces(self.indent * 2);
                self.xml_tag(label, X_OPENING | X_NOWHITESPACE);
                escape_xml(&mut self.out, value);
                self.xml_tag(label, X_CLOSING | X_NOWHITESPACE);
                self.out.push('\n');
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                escape_json(&mut self.out, label);
                self.out.push_str(": ");
                if numeric {
                    self.out.push_str(value);
                } else {
                    escape_json(&mut self.out, value);
                }
            }
            Format::Yaml => {
                self.yaml_line_starting();
                self.out.push_str(label);
                self.out.push_str(": ");
                if numeric {
                    self.out.push_str(value);
                } else {
                    escape_json(&mut self.out, value);
                }
            }
        }
    }

    /// `ExplainPropertyText`.
    pub(super) fn property_text(&mut self, label: &str, value: &str) {
        self.property(label, None, value, false);
    }

    /// `ExplainPropertyInteger`.
    pub(super) fn property_integer(&mut self, label: &str, unit: Option<&str>, value: i64) {
        self.property(label, unit, &value.to_string(), true);
    }

    /// `ExplainPropertyUInteger`.
    pub(super) fn property_uinteger(&mut self, label: &str, unit: Option<&str>, value: u64) {
        self.property(label, unit, &value.to_string(), true);
    }

    /// `ExplainPropertyFloat`, with `ndigits` digits after the point as `%.*f` writes them.
    pub(super) fn property_float(
        &mut self,
        label: &str,
        unit: Option<&str>,
        value: f64,
        ndigits: usize,
    ) {
        self.property(label, unit, &fixed(value, ndigits), true);
    }

    /// `ExplainPropertyBool`.
    pub(super) fn property_bool(&mut self, label: &str, value: bool) {
        self.property(label, None, if value { "true" } else { "false" }, true);
    }

    /// `ExplainOpenGroup`: opens a group of properties. `label` is the name of the group in JSON
    /// and YAML, and `labeled` says whether its members have labels, which makes it an object
    /// rather than an array in JSON.
    pub(super) fn open_group(&mut self, objtype: &str, label: Option<&str>, labeled: bool) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.xml_tag(objtype, X_OPENING);
                self.indent += 1;
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(2 * self.indent);
                if let Some(label) = label {
                    escape_json(&mut self.out, label);
                    self.out.push_str(": ");
                }
                self.out.push(if labeled { '{' } else { '[' });
                self.stack.push(0);
                self.indent += 1;
            }
            Format::Yaml => {
                self.yaml_line_starting();
                if let Some(label) = label {
                    self.out.push_str(label);
                    self.out.push_str(": ");
                    self.stack.push(1);
                } else {
                    self.out.push_str("- ");
                    self.stack.push(0);
                }
                self.indent += 1;
            }
        }
    }

    /// `ExplainCloseGroup`: closes what [`Self::open_group`] opened, with the same arguments.
    pub(super) fn close_group(&mut self, objtype: &str, labeled: bool) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.indent -= 1;
                self.xml_tag(objtype, X_CLOSING);
            }
            Format::Json => {
                self.indent -= 1;
                self.out.push('\n');
                self.spaces(2 * self.indent);
                self.out.push(if labeled { '}' } else { ']' });
                self.stack.pop();
            }
            Format::Yaml => {
                self.indent -= 1;
                self.stack.pop();
            }
        }
    }

    /// `ExplainBeginOutput`.
    pub(super) fn begin_output(&mut self) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.out.push_str("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n");
                self.indent += 1;
            }
            Format::Json => {
                self.out.push('[');
                self.stack.push(0);
                self.indent += 1;
            }
            Format::Yaml => self.stack.push(0),
        }
    }

    /// `ExplainEndOutput`.
    pub(super) fn end_output(&mut self) {
        match self.format {
            Format::Text => {}
            Format::Xml => {
                self.indent -= 1;
                self.out.push_str("</explain>");
            }
            Format::Json => {
                self.indent -= 1;
                self.out.push_str("\n]");
                self.stack.pop();
            }
            Format::Yaml => {
                self.stack.pop();
            }
        }
    }

    /// `ExplainXMLTag`: a tag with the characters that cannot be in an XML name changed to `-`.
    fn xml_tag(&mut self, tag: &str, flags: u8) {
        if flags & X_NOWHITESPACE == 0 {
            self.spaces(2 * self.indent);
        }
        self.out.push('<');
        if flags & X_CLOSING != 0 {
            self.out.push('/');
        }
        for c in tag.chars() {
            let valid = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
            self.out.push(if valid { c } else { '-' });
        }
        if flags & X_CLOSE_IMMEDIATE != 0 {
            self.out.push_str(" /");
        }
        self.out.push('>');
        if flags & X_NOWHITESPACE == 0 {
            self.out.push('\n');
        }
    }

    /// `ExplainIndentText`: the indent, when the text is at the start of a line.
    pub(super) fn indent_text(&mut self) {
        if self.out.is_empty() || self.out.ends_with('\n') {
            self.spaces(self.indent * 2);
        }
    }

    /// `ExplainJSONLineEnding`: the comma after the last member of the group, if it has one, and
    /// the line end.
    fn json_line_ending(&mut self) {
        match self.stack.last_mut() {
            Some(top) if *top != 0 => self.out.push(','),
            Some(top) => *top = 1,
            None => {}
        }
        self.out.push('\n');
    }

    /// `ExplainYAMLLineStarting`: a new line and the indent, except for the first member of a
    /// group, which goes on the line of its label.
    fn yaml_line_starting(&mut self) {
        match self.stack.last_mut() {
            Some(top) if *top == 0 => *top = 1,
            _ => {
                self.out.push('\n');
                self.spaces(self.indent * 2);
            }
        }
    }

    pub(super) fn spaces(&mut self, count: usize) {
        self.out.extend(std::iter::repeat_n(' ', count));
    }
}

/// `%.*f`: a number with `ndigits` digits after the point.
pub(super) fn fixed(value: f64, ndigits: usize) -> String {
    format!("{value:.ndigits$}")
}

/// `escape_json`: a JSON string, with the quotes. YAML uses the same function.
fn escape_json(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = std::fmt::Write::write_fmt(out, format_args!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `escape_xml`: the text of an XML element.
fn escape_xml(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#x0d;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_string_escapes_what_the_c_code_escapes() {
        let mut out = String::new();
        escape_json(&mut out, "a\"b\\c\nd\u{1}");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\u0001\"");
    }

    #[test]
    fn an_xml_tag_has_a_dash_for_a_space() {
        let mut writer = Writer::new(Format::Xml);
        writer.property_text("Node Type", "a<b");
        assert_eq!(writer.out, "<Node-Type>a&lt;b</Node-Type>\n");
    }

    #[test]
    fn a_json_group_puts_a_comma_between_its_members() {
        let mut writer = Writer::new(Format::Json);
        writer.begin_output();
        writer.open_group("Query", None, true);
        writer.property_integer("A", None, 1);
        writer.property_list("B", &["x".to_owned(), "y".to_owned()]);
        writer.close_group("Query", true);
        writer.end_output();
        assert_eq!(writer.out, "[\n  {\n    \"A\": 1,\n    \"B\": [\"x\", \"y\"]\n  }\n]");
    }

    #[test]
    fn a_yaml_group_starts_on_the_line_of_its_dash() {
        let mut writer = Writer::new(Format::Yaml);
        writer.begin_output();
        writer.open_group("Query", None, true);
        writer.open_group("Plan", Some("Plan"), true);
        writer.property_text("Node Type", "Result");
        writer.property_bool("Parallel Aware", false);
        writer.close_group("Plan", true);
        writer.close_group("Query", true);
        writer.end_output();
        assert_eq!(writer.out, "- Plan: \n    Node Type: \"Result\"\n    Parallel Aware: false");
    }
}
