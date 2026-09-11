//! The output modes.
//!
//! Every mode here is one DuckDB has, spelled the way DuckDB spells it, and producing the bytes
//! DuckDB produces. That is the whole point: a script that pipes `.mode csv` output into something
//! else is a script that has to keep working when the binary name changes, and a mode that is
//! nearly right is worse than one that is missing, because a missing one says so.
//!
//! `tests/shell.rs` holds the captured output of a real DuckDB binary for each of these and diffs
//! against it, so none of this is a claim.

use std::fmt::Write as _;

use rudb::QueryResult;
use rudb_common::{LogicalType, Value};

/// How many rows `duckbox` prints before it starts leaving some out.
const MAX_ROWS: usize = 40;

/// The three dots that stand in for the rows `duckbox` left out.
const ELIDED: usize = 3;

/// The narrowest field `line` mode right aligns a column name in.
const LINE_NAME_WIDTH: usize = 5;

/// What a result is printed as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// The default. A box with the column types under the names and a row count under the table.
    #[default]
    DuckBox,
    /// A box without the type row and without the row count.
    Box,
    /// The same table drawn in `+`, `-` and `|`.
    Table,
    /// A GitHub flavoured markdown table.
    Markdown,
    /// One `name = value` per line, a blank line between rows.
    Line,
    /// Values joined by the separator, which defaults to a pipe.
    List,
    /// Comma separated, with the quoting rules of RFC 4180.
    Csv,
    /// Tab separated.
    Tsv,
    /// One JSON array of objects.
    Json,
    /// One JSON object per line.
    JsonLines,
    /// Single quoted values, comma separated, in the spelling SQL wants.
    Quote,
    /// One `INSERT` statement per row.
    Insert,
    /// Table rows and cells as HTML, without the surrounding table element, which is what DuckDB
    /// emits.
    Html,
    /// One value per line, columns first, no decoration at all.
    Ascii,
    /// Space padded columns under a dashed rule.
    Column,
    /// Nothing at all, for timing a query without paying to print it.
    Trash,
}

impl Format {
    /// The mode of that name, or `None` if there is no such mode.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "duckbox" => Self::DuckBox,
            "box" => Self::Box,
            "table" => Self::Table,
            "markdown" => Self::Markdown,
            "line" | "lines" => Self::Line,
            "list" => Self::List,
            "csv" => Self::Csv,
            "tabs" | "tsv" => Self::Tsv,
            "json" => Self::Json,
            "jsonlines" | "ndjson" => Self::JsonLines,
            "quote" => Self::Quote,
            "insert" => Self::Insert,
            "html" => Self::Html,
            "ascii" => Self::Ascii,
            "column" => Self::Column,
            "trash" => Self::Trash,
            _ => return None,
        })
    }

    /// The name this mode answers to, which is what `.show` prints.
    pub fn name(self) -> &'static str {
        match self {
            Self::DuckBox => "duckbox",
            Self::Box => "box",
            Self::Table => "table",
            Self::Markdown => "markdown",
            Self::Line => "line",
            Self::List => "list",
            Self::Csv => "csv",
            Self::Tsv => "tabs",
            Self::Json => "json",
            Self::JsonLines => "jsonlines",
            Self::Quote => "quote",
            Self::Insert => "insert",
            Self::Html => "html",
            Self::Ascii => "ascii",
            Self::Column => "column",
            Self::Trash => "trash",
        }
    }

    /// What `.mode` sets the column separator to, since setting the mode resets it.
    fn separator(self) -> &'static str {
        match self {
            Self::Csv | Self::Quote => ",",
            Self::Tsv => "\t",
            Self::Ascii => "\u{1f}",
            _ => "|",
        }
    }

    /// What `.mode` sets the row separator to.
    ///
    /// CSV gets `\r\n` because RFC 4180 says so and because DuckDB does it, which surprises people
    /// reading the file on a Unix machine and is nonetheless what a CSV is.
    fn newline(self) -> &'static str {
        match self {
            Self::Csv => "\r\n",
            Self::Ascii => "\u{1e}",
            _ => "\n",
        }
    }
}

/// Everything about how output is printed, which is what the dot commands change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The output mode.
    pub format: Format,
    /// Whether to print the column names.
    pub header: bool,
    /// What goes between two values in the separated modes.
    pub separator: String,
    /// What goes between two rows in the separated modes.
    pub newline: String,
    /// What a null prints as in the modes that do not have a spelling of their own for it.
    pub nullvalue: String,
    /// The table name `.mode insert` puts in the statements it writes.
    pub table: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: Format::DuckBox,
            header: true,
            separator: "|".to_string(),
            newline: "\n".to_string(),
            nullvalue: "NULL".to_string(),
            table: "table".to_string(),
        }
    }
}

impl Settings {
    /// Switches mode, resetting both separators to that mode's defaults.
    ///
    /// Resetting is DuckDB's behaviour and it surprises people, so it is worth saying why it is
    /// right: `.mode csv` means "write me a CSV", and a pipe separator left over from an earlier
    /// `.mode list` would produce a file that is not one. The header setting is deliberately left
    /// alone, which is also DuckDB's behaviour and was checked against the binary rather than
    /// guessed, because `.headers off` is a thing somebody says once and expects to stay said.
    pub fn set_format(&mut self, format: Format) {
        self.format = format;
        self.separator = format.separator().to_string();
        self.newline = format.newline().to_string();
    }
}

/// How `.show` spells a separator, which is with the escapes rather than the bytes.
pub fn escaped(text: &str) -> String {
    let mut out = String::new();
    for character in text.chars() {
        match character {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\{:03o}", other as u32);
            }
            other => out.push(other),
        }
    }
    out
}

/// Prints a result.
pub fn render(result: &QueryResult, settings: &Settings) -> String {
    if result.width() == 0 {
        return String::new();
    }
    let cells = cells(result, settings);
    match settings.format {
        Format::DuckBox => duckbox(result, &cells),
        Format::Box => boxed(result, &cells, BOX_GLYPHS),
        Format::Table => boxed(result, &cells, TABLE_GLYPHS),
        Format::Markdown => markdown(result, &cells),
        Format::Line => line(result, &cells),
        Format::List | Format::Csv | Format::Tsv => separated(result, &cells, settings),
        Format::Json => json(result, settings, true),
        Format::JsonLines => json(result, settings, false),
        Format::Quote => quote(result, settings),
        Format::Insert => insert(result, settings),
        Format::Html => html(result, &cells, settings),
        Format::Ascii => ascii(result, &cells, settings),
        Format::Column => column(result, &cells),
        Format::Trash => String::new(),
    }
}

/// Every value as the text it prints as, which the table modes then measure and pad.
fn cells(result: &QueryResult, settings: &Settings) -> Vec<Vec<String>> {
    (0..result.len())
        .map(|row| {
            (0..result.width())
                .map(|column| cell(&result.value_at(row, column), settings))
                .collect()
        })
        .collect()
}

/// One value as text.
fn cell(value: &Value, settings: &Settings) -> String {
    match value {
        Value::Null => settings.nullvalue.clone(),
        other => other.to_string(),
    }
}

/// The name `duckbox` puts under a column heading.
///
/// These are DuckDB's internal type names rather than the SQL spelling, which is why an `INTEGER`
/// column says `int32`. Lives here rather than on [`LogicalType`] because the shell is the only
/// thing that wants them; it moves down to `rudb-common` the day a second surface does.
///
/// The last arm is there because [`LogicalType`] is `non_exhaustive`, so a type added below this
/// crate compiles rather than breaking the build. It prints the lowercased SQL name, which is right
/// for most of them and is at worst a name a reader can still recognize.
fn type_name(ty: &LogicalType) -> String {
    match ty {
        LogicalType::Null => "\"NULL\"".to_string(),
        LogicalType::Boolean => "boolean".to_string(),
        LogicalType::TinyInt => "int8".to_string(),
        LogicalType::SmallInt => "int16".to_string(),
        LogicalType::Integer => "int32".to_string(),
        LogicalType::BigInt => "int64".to_string(),
        LogicalType::HugeInt => "int128".to_string(),
        LogicalType::UTinyInt => "uint8".to_string(),
        LogicalType::USmallInt => "uint16".to_string(),
        LogicalType::UInteger => "uint32".to_string(),
        LogicalType::UBigInt => "uint64".to_string(),
        LogicalType::UHugeInt => "uint128".to_string(),
        LogicalType::Float => "float".to_string(),
        LogicalType::Double => "double".to_string(),
        LogicalType::Decimal { width, scale } => format!("decimal({width},{scale})"),
        LogicalType::Varchar => "varchar".to_string(),
        LogicalType::Blob => "blob".to_string(),
        LogicalType::Bit => "bit".to_string(),
        LogicalType::Uuid => "uuid".to_string(),
        LogicalType::Date => "date".to_string(),
        LogicalType::Time => "time".to_string(),
        LogicalType::TimeTz => "time with time zone".to_string(),
        LogicalType::Timestamp => "timestamp".to_string(),
        LogicalType::TimestampS => "timestamp_s".to_string(),
        LogicalType::TimestampMs => "timestamp_ms".to_string(),
        LogicalType::TimestampNs => "timestamp_ns".to_string(),
        LogicalType::TimestampTz => "timestamp with time zone".to_string(),
        LogicalType::Interval => "interval".to_string(),
        LogicalType::List(inner) | LogicalType::Array(inner, _) => {
            format!("{}[]", type_name(inner))
        }
        LogicalType::Map(key, value) => format!("map({}, {})", type_name(key), type_name(value)),
        LogicalType::Struct(fields) => {
            let inner: Vec<String> =
                fields.iter().map(|field| format!("{} {}", field.name, field.ty)).collect();
            format!("struct({})", inner.join(", ")).to_lowercase()
        }
        LogicalType::Union(fields) => {
            let inner: Vec<String> =
                fields.iter().map(|field| format!("{} {}", field.name, field.ty)).collect();
            format!("union({})", inner.join(", ")).to_lowercase()
        }
        other => other.to_string().to_lowercase(),
    }
}

/// How wide a string is on a terminal.
///
/// Character count, not grapheme clusters and not East Asian width. That is wrong for a string
/// holding a combining mark or a full width character and it is right for everything in the
/// benchmarks and the corpus, and fixing it properly means a Unicode width table, which is a
/// dependency and a decision of its own. Named here so it is a known gap rather than a surprise.
fn width(text: &str) -> usize {
    text.chars().count()
}

/// Which rows a `duckbox` table shows, and where the dots go.
///
/// Only elide when eliding actually saves lines. Replacing 41 rows with 20, three dots and 20 is
/// longer than printing all 41, and DuckDB knows that, so the cut is at `MAX_ROWS + ELIDED`.
fn shown(rows: usize) -> Option<(usize, usize)> {
    if rows > MAX_ROWS + ELIDED { Some((MAX_ROWS / 2, MAX_ROWS / 2)) } else { None }
}

/// Pads `text` to `size`, to the right if `right` and to the left otherwise.
fn pad(text: &str, size: usize, right: bool) -> String {
    let missing = size.saturating_sub(width(text));
    if right {
        format!("{}{}", " ".repeat(missing), text)
    } else {
        format!("{}{}", text, " ".repeat(missing))
    }
}

/// Pads `text` to `size` with the extra space split, the odd one going right.
fn centre(text: &str, size: usize) -> String {
    let missing = size.saturating_sub(width(text));
    let left = missing / 2;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(missing - left))
}

/// The widest each column has to be, over the heading, the type and every value shown.
fn widths(result: &QueryResult, cells: &[Vec<String>], types: bool) -> Vec<usize> {
    (0..result.width())
        .map(|column| {
            let mut size = width(&result.names()[column]);
            if types {
                size = size.max(width(&type_name(&result.types()[column])));
            }
            for row in cells {
                size = size.max(width(&row[column]));
            }
            size
        })
        .collect()
}

/// The characters a box is drawn with.
struct Glyphs {
    top: [&'static str; 4],
    middle: [&'static str; 4],
    bottom: [&'static str; 4],
    vertical: &'static str,
}

const BOX_GLYPHS: Glyphs = Glyphs {
    top: ["┌", "─", "┬", "┐"],
    middle: ["├", "─", "┼", "┤"],
    bottom: ["└", "─", "┴", "┘"],
    vertical: "│",
};

const TABLE_GLYPHS: Glyphs = Glyphs {
    top: ["+", "-", "+", "+"],
    middle: ["+", "-", "+", "+"],
    bottom: ["+", "-", "+", "+"],
    vertical: "|",
};

/// One horizontal rule of a box.
fn rule(widths: &[usize], glyphs: &[&str; 4]) -> String {
    let parts: Vec<String> = widths.iter().map(|size| glyphs[1].repeat(size + 2)).collect();
    format!("{}{}{}", glyphs[0], parts.join(glyphs[2]), glyphs[3])
}

/// One row of a box, each cell already padded.
fn row(parts: &[String], vertical: &str) -> String {
    let mut out = String::from(vertical);
    for part in parts {
        let _ = write!(out, " {part} {vertical}");
    }
    out
}

/// The default mode: a box, a type row, and a count under it.
fn duckbox(result: &QueryResult, cells: &[Vec<String>]) -> String {
    let mut sizes = widths(result, cells, true);
    let footer = footer_text(result, cells.len());
    // The table cannot be narrower than the count printed under it, which is the only reason a
    // one column table of `int32` comes out eight wide rather than seven.
    if let Some(first) = footer.first() {
        let total: usize = sizes.iter().map(|size| size + 2).sum::<usize>() + sizes.len() - 1;
        let needed = width(first) + 2;
        if let Some(last) = sizes.last_mut() {
            *last += needed.saturating_sub(total);
        }
    }
    let right: Vec<bool> = result.types().iter().map(LogicalType::is_numeric).collect();
    let mut out = String::new();
    let _ = writeln!(out, "{}", rule(&sizes, &BOX_GLYPHS.top));
    let heads: Vec<String> =
        result.names().iter().zip(&sizes).map(|(name, size)| centre(name, *size)).collect();
    let _ = writeln!(out, "{}", row(&heads, BOX_GLYPHS.vertical));
    let types: Vec<String> =
        result.types().iter().zip(&sizes).map(|(ty, size)| centre(&type_name(ty), *size)).collect();
    let _ = writeln!(out, "{}", row(&types, BOX_GLYPHS.vertical));
    if !cells.is_empty() {
        let _ = writeln!(out, "{}", rule(&sizes, &BOX_GLYPHS.middle));
        write_rows(&mut out, cells, &sizes, &right, BOX_GLYPHS.vertical);
    }
    let _ = writeln!(out, "{}", rule(&sizes, &BOX_GLYPHS.bottom));
    let total: usize = sizes.iter().map(|size| size + 2).sum::<usize>() + sizes.len() - 1;
    for text in footer {
        let left = (total.saturating_sub(width(&text))) / 2 + 1;
        let _ = writeln!(out, "{}{}", " ".repeat(left), text);
    }
    out
}

/// The lines printed under a `duckbox` table, which is nothing at all for most results.
///
/// A count appears when the result is empty, because an empty box says nothing on its own, and
/// when rows were left out, because a table that is not all of the answer has to say so. A result
/// of three rows prints three rows and no commentary.
fn footer_text(result: &QueryResult, rows: usize) -> Vec<String> {
    if rows == 0 {
        return vec!["0 rows".to_string()];
    }
    let Some((head, tail)) = shown(result.len()) else {
        return Vec::new();
    };
    let counted = format!("{} rows", result.len());
    let elided = format!("({} shown)", head + tail);
    vec![counted, elided]
}

/// The rows of a box, with the dots in the middle if some were left out.
fn write_rows(
    out: &mut String,
    cells: &[Vec<String>],
    sizes: &[usize],
    right: &[bool],
    vertical: &str,
) {
    let dots = shown(cells.len());
    for (at, values) in cells.iter().enumerate() {
        if let Some((head, tail)) = dots {
            if at == head {
                for _ in 0..ELIDED {
                    let parts: Vec<String> = sizes.iter().map(|size| centre("·", *size)).collect();
                    let _ = writeln!(out, "{}", row(&parts, vertical));
                }
            }
            if at >= head && at < cells.len() - tail {
                continue;
            }
        }
        let parts: Vec<String> = values
            .iter()
            .zip(sizes)
            .zip(right)
            .map(|((value, size), right)| pad(value, *size, *right))
            .collect();
        let _ = writeln!(out, "{}", row(&parts, vertical));
    }
}

/// `.mode box` and `.mode table`, which are the same table without the types and without the count.
fn boxed(result: &QueryResult, cells: &[Vec<String>], glyphs: Glyphs) -> String {
    let sizes = widths(result, cells, false);
    // Left for every column, numbers included. Only duckbox right aligns numbers, which reads oddly
    // until you notice that box and table are the modes DuckDB inherited from SQLite and duckbox is
    // the one it wrote.
    let right = vec![false; result.width()];
    let mut out = String::new();
    let _ = writeln!(out, "{}", rule(&sizes, &glyphs.top));
    let heads: Vec<String> =
        result.names().iter().zip(&sizes).map(|(name, size)| centre(name, *size)).collect();
    let _ = writeln!(out, "{}", row(&heads, glyphs.vertical));
    let _ = writeln!(out, "{}", rule(&sizes, &glyphs.middle));
    write_rows(&mut out, cells, &sizes, &right, glyphs.vertical);
    let _ = writeln!(out, "{}", rule(&sizes, &glyphs.bottom));
    out
}

/// `.mode markdown`, where the alignment lives in the rule rather than in the padding.
fn markdown(result: &QueryResult, cells: &[Vec<String>]) -> String {
    let sizes = widths(result, cells, false);
    // A numeric column says it is right aligned in the rule and is still padded on the right in the
    // cells, which is markdown's whole trick: the renderer downstream does the aligning.
    let numeric: Vec<bool> = result.types().iter().map(LogicalType::is_numeric).collect();
    let right = vec![false; result.width()];
    let mut out = String::new();
    let heads: Vec<String> =
        result.names().iter().zip(&sizes).map(|(name, size)| centre(name, *size)).collect();
    let _ = writeln!(out, "{}", row(&heads, "|"));
    let rules: Vec<String> = sizes
        .iter()
        .zip(&numeric)
        .map(
            |(size, right)| {
                if *right { format!("{}:", "-".repeat(size + 1)) } else { "-".repeat(size + 2) }
            },
        )
        .collect();
    let _ = writeln!(out, "|{}|", rules.join("|"));
    let mut rows = String::new();
    write_rows(&mut rows, cells, &sizes, &right, "|");
    out.push_str(&rows);
    out
}

/// `.mode line`, one `name = value` per line.
///
/// The names are right aligned in a field at least five wide, which is the width SQLite picked and
/// DuckDB kept. A one letter column therefore gets four spaces in front of it and looks deliberate
/// rather than broken, which is presumably the point.
fn line(result: &QueryResult, cells: &[Vec<String>]) -> String {
    let widest =
        result.names().iter().map(|name| width(name)).max().unwrap_or(0).max(LINE_NAME_WIDTH);
    let mut out = String::new();
    for (at, values) in cells.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        for (name, value) in result.names().iter().zip(values) {
            let _ = writeln!(out, "{} = {}", pad(name, widest, true), value);
        }
    }
    out
}

/// `.mode list`, `.mode csv` and `.mode tabs`, which differ only in the separator and the quoting.
fn separated(result: &QueryResult, cells: &[Vec<String>], settings: &Settings) -> String {
    let quoted = settings.format == Format::Csv;
    let mut out = String::new();
    if settings.header {
        let heads: Vec<String> = result
            .names()
            .iter()
            .map(|name| if quoted { csv(name, settings) } else { name.clone() })
            .collect();
        out.push_str(&heads.join(&settings.separator));
        out.push_str(&settings.newline);
    }
    for values in cells {
        let parts: Vec<String> = values
            .iter()
            .map(|value| if quoted { csv(value, settings) } else { value.clone() })
            .collect();
        out.push_str(&parts.join(&settings.separator));
        out.push_str(&settings.newline);
    }
    out
}

/// One CSV field, quoted when RFC 4180 says it has to be.
fn csv(text: &str, settings: &Settings) -> String {
    let awkward = text.contains(&settings.separator)
        || text.contains('"')
        || text.contains('\n')
        || text.contains('\r');
    if awkward { format!("\"{}\"", text.replace('"', "\"\"")) } else { text.to_string() }
}

/// `.mode json` and `.mode jsonlines`.
fn json(result: &QueryResult, settings: &Settings, array: bool) -> String {
    let mut out = String::new();
    // row at a time: a printed row is a row, and the thing being fed is a terminal or a pipe, so
    // the cost of the loop is nowhere near the cost of the bytes leaving the process.
    for row in 0..result.len() {
        let parts: Vec<String> = (0..result.width())
            .map(|column| {
                format!(
                    "{}:{}",
                    json_string(&result.names()[column]),
                    json_value(&result.value_at(row, column))
                )
            })
            .collect();
        let object = format!("{{{}}}", parts.join(","));
        if array {
            if row == 0 {
                out.push('[');
            }
            out.push_str(&object);
            if row + 1 < result.len() {
                out.push_str(",\n");
            } else {
                out.push_str("]\n");
            }
        } else {
            let _ = writeln!(out, "{object}");
        }
    }
    if array && result.is_empty() {
        out.push_str("[]\n");
    }
    let _ = settings;
    out
}

/// One JSON string, escaped.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", other as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// One JSON value, which is a number for a number and a string for everything that is not one.
fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::List { values, .. } => {
            let parts: Vec<String> = values.iter().map(json_value).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Struct(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(name, value)| format!("{}:{}", json_string(name), json_value(value)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        other if is_number(other) => other.to_string(),
        other => json_string(&other.to_string()),
    }
}

/// Whether a value prints as a JSON number rather than as a JSON string.
fn is_number(value: &Value) -> bool {
    matches!(
        value,
        Value::TinyInt(_)
            | Value::SmallInt(_)
            | Value::Integer(_)
            | Value::BigInt(_)
            | Value::HugeInt(_)
            | Value::UTinyInt(_)
            | Value::USmallInt(_)
            | Value::UInteger(_)
            | Value::UBigInt(_)
            | Value::UHugeInt(_)
            | Value::Float(_)
            | Value::Double(_)
            | Value::Decimal { .. }
    )
}

/// `.mode quote`, which is the spelling a value has inside a SQL statement.
fn quote(result: &QueryResult, settings: &Settings) -> String {
    let mut out = String::new();
    if settings.header {
        let heads: Vec<String> =
            result.names().iter().map(|name| format!("'{}'", name.replace('\'', "''"))).collect();
        out.push_str(&heads.join(&settings.separator));
        out.push_str(&settings.newline);
    }
    // row at a time: the output is one quoted row per line, so there is nothing to batch.
    for row in 0..result.len() {
        let parts: Vec<String> =
            (0..result.width()).map(|column| sql_literal(&result.value_at(row, column))).collect();
        out.push_str(&parts.join(&settings.separator));
        out.push_str(&settings.newline);
    }
    out
}

/// `.mode insert`, one statement per row.
fn insert(result: &QueryResult, settings: &Settings) -> String {
    let mut out = String::new();
    let columns = result.names().join(",");
    // row at a time: the output is one INSERT statement per row, which is the shape of the mode.
    for row in 0..result.len() {
        let parts: Vec<String> =
            (0..result.width()).map(|column| sql_literal(&result.value_at(row, column))).collect();
        let _ = writeln!(
            out,
            "INSERT INTO \"{}\"({}) VALUES({});",
            settings.table,
            columns,
            parts.join(",")
        );
    }
    out
}

/// A value as it would be written in SQL.
fn sql_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        other if is_number(other) => other.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

/// `.mode html`, the rows without the table around them, which is what DuckDB emits.
fn html(result: &QueryResult, cells: &[Vec<String>], settings: &Settings) -> String {
    let mut out = String::new();
    if settings.header {
        out.push_str("<tr>");
        for name in result.names() {
            let _ = writeln!(out, "<th>{}</th>", escape(name));
        }
        out.push_str("</tr>\n");
    }
    for values in cells {
        out.push_str("<tr>");
        for value in values {
            let _ = writeln!(out, "<td>{}</td>", escape(value));
        }
        out.push_str("</tr>\n");
    }
    out
}

/// The four characters that cannot appear raw in HTML text.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `.mode ascii`, every value on its own line with no decoration.
fn ascii(result: &QueryResult, cells: &[Vec<String>], settings: &Settings) -> String {
    let mut out = String::new();
    if settings.header {
        for name in result.names() {
            let _ = writeln!(out, "{name}");
        }
    }
    for values in cells {
        for value in values {
            let _ = writeln!(out, "{value}");
        }
    }
    let _ = settings;
    out
}

/// `.mode column`, space padded under a dashed rule.
fn column(result: &QueryResult, cells: &[Vec<String>]) -> String {
    let sizes = widths(result, cells, false);
    let mut out = String::new();
    let heads: Vec<String> =
        result.names().iter().zip(&sizes).map(|(name, size)| pad(name, *size, false)).collect();
    let _ = writeln!(out, "{}", heads.join("  "));
    let rules: Vec<String> = sizes.iter().map(|size| "-".repeat(*size)).collect();
    let _ = writeln!(out, "{}", rules.join("  "));
    for values in cells {
        let parts: Vec<String> =
            values.iter().zip(&sizes).map(|(value, size)| pad(value, *size, false)).collect();
        let _ = writeln!(out, "{}", parts.join("  "));
    }
    out
}
