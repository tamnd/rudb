//! Where a native file's bytes went, per column and per kind of byte.
//!
//! # Why this exists
//!
//! ClickBench `hits` is 99,997,497 rows. DuckDB stores it in 20,462,972,928 bytes. rudb stores the
//! same rows from the same `hits.parquet` in 44,984,691,900, which is 2.2 times as many. That is a
//! cold scan reading twice the bytes before a single one of them is decoded, and it is half of the
//! ten times less resource the project is aiming at, and until today nobody could say which column
//! it was in.
//!
//! The encoder is not short of tricks. Integers get frame of reference, delta, run length,
//! dictionary and sparse, cascaded three deep, and strings get FSST, dictionary, front coding and a
//! copy matcher on top of the same cascade. So 2.2 times is not a missing codec. It is either a few
//! columns doing something bad, or it is a fixed cost paid too many times, and those two look
//! completely different in this table.
//!
//! # What it costs to ask
//!
//! Nothing. Every number comes out of the committed directory, which [`Reader::open`] reads anyway,
//! so this is one directory read whether the file is empty or 45 GB. A tool that had to read the
//! pages would take minutes on the only file worth pointing it at, and a tool that takes minutes is
//! a tool nobody runs twice.
//!
//! # What is not a column
//!
//! Three things are charged on their own rather than shared out. The stripe index page holds a
//! length and a checksum for every part of every column and could be split per column, the
//! directory holds a zone bound and a span per column per stripe and could be too, and the header
//! cannot be split by anything. Splitting one and not the others would print a table whose columns
//! added up to the file, which is exactly the impression to avoid, because the gap between the
//! columns and the file is the thing this was written to find.

use std::path::Path;

use rudb_native::{ColumnLayout, Layout, Reader};

/// Prints the layout of every file named on the command line.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let mut paths: Vec<&String> = Vec::new();
    let mut all = false;
    for arg in args {
        if arg == "--all" { all = true } else { paths.push(arg) }
    }
    if paths.is_empty() {
        return Err("cargo xtask native <file.db> [--all], a rudb native file".to_string());
    }
    for path in paths {
        let reader = Reader::open(Path::new(path)).map_err(|error| format!("{path}: {error}"))?;
        report(path, &reader.layout(), all);
    }
    Ok(())
}

fn report(path: &str, layout: &Layout, all: bool) {
    let rows = layout.rows.max(1) as f64;
    println!("{path}");
    println!(
        "  {} bytes, {} rows, {} columns, {} stripes, {} parts, {:.0} rows a part",
        thousands(layout.file),
        thousands(layout.rows as u64),
        layout.columns.len(),
        thousands(layout.stripes as u64),
        thousands(layout.parts as u64),
        layout.rows as f64 / layout.parts.max(1) as f64
    );
    println!();

    let columns = layout.columns_total();
    let pages = total(layout, |column| column.pages);
    let memberships = total(layout, |column| column.memberships);
    let sieves = total(layout, |column| column.sieves);
    let dictionaries = total(layout, |column| column.dictionary);
    println!("  {:>16}  {:>6}  {:>8}  what", "bytes", "share", "per row");
    for (bytes, what) in [
        (pages, "column pages, the encoded data"),
        (dictionaries, "table wide dictionaries"),
        (memberships, "exact code membership pages"),
        (sieves, "membership sieve pages"),
        (layout.indexes, "stripe index pages, per part lengths and checksums"),
        (layout.directory, "the committed directory"),
        (layout.header, "the header"),
        (layout.unaccounted(), "not accounted for, which is any earlier snapshot"),
    ] {
        line(bytes, layout.file, rows, what);
    }
    println!();
    println!("  columns together are {} bytes", thousands(columns));
    println!();

    let mut sorted: Vec<&ColumnLayout> = layout.columns.iter().collect();
    sorted.sort_by_key(|column| std::cmp::Reverse(column.total()));
    let shown = if all { sorted.len() } else { sorted.len().min(20) };
    println!(
        "  {:>16}  {:>8}  {:>16}  {:>14}  {:>12}  column",
        "total", "per row", "pages", "dictionary", "sieve"
    );
    for column in sorted.iter().take(shown) {
        println!(
            "  {:>16}  {:>8.2}  {:>16}  {:>14}  {:>12}  {} {}",
            thousands(column.total()),
            column.total() as f64 / rows,
            thousands(column.pages),
            thousands(column.dictionary),
            thousands(column.sieves.saturating_add(column.memberships)),
            column.name,
            column.kind
        );
    }
    if shown < sorted.len() {
        println!("  and {} more, --all prints every one", sorted.len() - shown);
    }
    println!();
}

fn total(layout: &Layout, of: impl Fn(&ColumnLayout) -> u64) -> u64 {
    layout.columns.iter().map(of).fold(0, u64::saturating_add)
}

fn line(bytes: u64, file: u64, rows: f64, what: &str) {
    let share = if file == 0 { 0.0 } else { bytes as f64 * 100.0 / file as f64 };
    println!("  {:>16}  {share:>5.1}%  {:>8.2}  {what}", thousands(bytes), bytes as f64 / rows);
}

/// A byte count with separators, because the difference between 44 GB and 4.4 GB is one character
/// and this table exists to be read off a terminal.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::thousands;

    #[test]
    fn a_byte_count_is_grouped_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1), "1");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(44_984_691_900), "44,984,691,900");
    }
}
