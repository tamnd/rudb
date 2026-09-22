//! What a key map costs to build and to keep, per table, over a real file.
//!
//! # Why this exists
//!
//! `spec/graph/10-milestones.md` gives G1 one exit measurement: a native TPC-H SF10 file with key
//! maps on all eight tables, under the budget, with the build time and the bytes reported per
//! table. Two of those three numbers are not in `rudb_links()` and cannot be. It reports what a
//! file holds, and a build that the budget of section 3.7 turned away holds nothing, so the cost of
//! the structure that was not kept has nowhere to be reported from. That number is the one that
//! decides whether the budget is set right, so it needs a tool that watches the build rather than
//! reading the result.
//!
//! # What it costs to ask
//!
//! One pass over each named column, which is what building a key map costs anyway. This is the
//! build, not a simulation of it: the file it is pointed at comes back with the sections in it, and
//! running it twice is not twice the work because the second run replaces what the first one wrote.
//!
//! # Why a column and not a relationship
//!
//! A key map is over a column. A relationship names two of them and only the parent side gets a
//! map, so pointing this at relationships would make eight tables into a list of the joins somebody
//! happened to write down, and two TPC-H tables are never anybody's parent. The measurement wants
//! every table, so the argument is a column.

use std::path::Path;
use std::time::Duration;

use rudb_native::Catalog;
use rudb_native::graph::{Built, build_key_maps};

/// Builds a key map over every column named on the command line, then prints what each one cost.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let mut path: Option<&String> = None;
    let mut wanted: Vec<(String, String)> = Vec::new();
    for arg in args {
        match arg.split_once('.') {
            Some((table, column)) if path.is_some() => {
                wanted.push((table.to_string(), column.to_string()));
            }
            _ if path.is_none() => path = Some(arg),
            _ => return Err(format!("{arg} is not table.column")),
        }
    }
    let Some(path) = path else {
        return Err("cargo xtask graph <file.db> <table.column>...".to_string());
    };
    if wanted.is_empty() {
        return Err("no column named, so there is nothing to build".to_string());
    }
    let file = Path::new(path);
    let catalog = Catalog::open(file).map_err(|error| format!("{path}: {error}"))?;
    println!("{path}");
    println!(
        "  {:<12} {:<16} {:>12} {:>9} {:>11} {:>12} {:>6} {:>9}  kept",
        "table", "column", "rows", "form", "bytes", "column", "share", "build"
    );
    let mut bytes = 0;
    let mut spent = Duration::ZERO;
    for (table, column) in &wanted {
        let reader = catalog.table(table).map_err(|error| format!("{table}: {error}"))?;
        let at = reader
            .table()
            .fields()
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(column))
            .ok_or_else(|| format!("{table} has no column {column}"))?;
        drop(reader);
        let built = build_key_maps(file, table, &[at])
            .map_err(|error| format!("{table}.{column}: {error}"))?;
        for one in &built {
            report(table, column, one);
            spent += one.build;
            if one.built {
                bytes += one.bytes;
            }
        }
    }
    println!("  {} bytes kept, built in {:.2}s", thousands(bytes as u64), spent.as_secs_f64());
    Ok(())
}

/// One row, which is one column's key map.
fn report(table: &str, column: &str, built: &Built) {
    let share = if built.column_bytes == 0 {
        f64::INFINITY
    } else {
        100.0 * built.bytes as f64 / built.column_bytes as f64
    };
    println!(
        "  {:<12} {:<16} {:>12} {:>9} {:>11} {:>12} {:>5.1}% {:>8.2}s  {}",
        table,
        column,
        thousands(built.rows),
        // A map that was declined for a repeat has a form the way an empty box has a shape, so it
        // is not printed as one.
        if built.distinct { built.form.label() } else { "none" },
        thousands(built.bytes as u64),
        thousands(built.column_bytes),
        share,
        built.build.as_secs_f64(),
        kept(built),
    );
}

/// Why a map is in the file or is not, which is the column somebody reads this table for.
///
/// A map that does not fit is the finding, not the failure. Section 3.7 says a relationship that
/// does not fit is recorded as not built with its size, and the size is the number beside it here.
fn kept(built: &Built) -> &'static str {
    match (built.built, built.distinct) {
        (true, _) => "yes",
        (false, true) => "no, over budget",
        (false, false) => "no, the key repeats",
    }
}

/// A number with separators, since these run to ten digits and are read by eye.
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
