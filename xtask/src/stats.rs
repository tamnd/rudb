//! What a table's statistics cost to build and to keep, over a real file.
//!
//! # Why this exists
//!
//! `spec/stats/10-milestones.md` gives G2 an exit measurement that nothing already in the tree can
//! produce: statistics sections under two percent of the stored column bytes and the build under ten
//! percent of the native write time, on TPC-H SF100 and on ClickBench at a hundred million rows.
//! Neither number can be read off a finished file. A column the budget of section 3.8 turned away
//! leaves nothing behind, so what it would have cost has nowhere to be reported from, and that is
//! precisely the number that says whether two percent is set right.
//!
//! This is `xtask graph` for the other document, and it is deliberately the same shape. One table
//! per run, every column of it that can be summarized, one row each and a total.
//!
//! Half of the exit criterion is here and half of it is not. The bytes and the share of column
//! bytes are what this prints. The build time is printed too, but the ten percent it has to fit
//! inside is ten percent of the write, and this tool is pointed at a file that is already written,
//! so it has no write to measure. The comparison is made against the ingest time the benchmark
//! harness already reports for the same file.
//!
//! # A table and not a column
//!
//! `xtask graph` takes columns because a key map is over a column and only the parent side of a
//! relationship gets one. The statistics budget is over the table, every column can have a summary,
//! and the exit criterion is about all sixteen `lineitem` columns together rather than about any one
//! of them. So the argument is a table and the tool summarizes all of it.
//!
//! # `--stripes`
//!
//! Section 3.8's rule is that per stripe sketches go only to the columns that get read, and the
//! arithmetic behind the rule is the thing worth being able to check rather than to quote. Passing
//! `--stripes` promotes every column, which is the case the spec says does not fit, so the run
//! prints the number the rule exists because of. Without it the promoted set is the one the file
//! itself names, which is what a real checkpoint would write.

use std::path::Path;
use std::time::Duration;

use rudb_native::Catalog;
use rudb_native::stats::{BUDGET_SHARE, Built, build_stats_for, read_columns, summarizable};

/// Builds the statistics for every summarizable column of each named table and prints what it cost.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let mut path: Option<&String> = None;
    let mut tables: Vec<&String> = Vec::new();
    let mut every_stripe = false;
    for arg in args {
        match arg.as_str() {
            "--stripes" => every_stripe = true,
            _ if path.is_none() => path = Some(arg),
            _ => tables.push(arg),
        }
    }
    let Some(path) = path else {
        return Err("cargo xtask stats <file.db> <table>... [--stripes]".to_string());
    };
    if tables.is_empty() {
        return Err("no table named, so there is nothing to summarize".to_string());
    }
    let file = Path::new(path);
    println!("{path}");
    println!(
        "  {:<12} {:<18} {:>13} {:>12} {:>7} {:>9} {:>8} {:>7} {:>8}  kept",
        "table", "column", "rows", "distinct", "exact", "summary", "sketch", "stripes", "build"
    );
    for table in tables {
        let catalog = Catalog::open(file).map_err(|error| format!("{path}: {error}"))?;
        let reader = catalog.table(table).map_err(|error| format!("{table}: {error}"))?;
        let names: Vec<String> =
            reader.table().fields().iter().map(|field| field.name.clone()).collect();
        let wanted: Vec<usize> = reader
            .table()
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, field)| summarizable(&field.ty))
            .map(|(at, _)| at)
            .collect();
        // Every column, or the ones the file itself says something was declared over. Read before
        // the build, because the build replaces the sections it reads this off.
        let promoted = if every_stripe { wanted.clone() } else { read_columns(&reader) };
        let column_bytes = reader.layout().columns_total();
        drop(reader);
        drop(catalog);
        if wanted.is_empty() {
            return Err(format!("{table} has no column that can be summarized"));
        }
        let built = build_stats_for(file, table, &wanted, &promoted, BUDGET_SHARE)
            .map_err(|error| format!("{table}: {error}"))?;
        let mut kept = 0;
        let mut measured = 0;
        let mut spent = Duration::ZERO;
        for one in &built {
            report(table, &names[one.column], one);
            spent += one.build;
            measured += one.bytes();
            if one.built {
                kept += one.bytes();
            }
        }
        // The share is of what was kept, and the one beside it is of what was built, because the
        // two differ exactly when the budget bound and that is the case worth seeing. A run where
        // they are equal is a run where nothing was turned away.
        let share = |bytes: usize| {
            if column_bytes == 0 {
                f64::INFINITY
            } else {
                100.0 * bytes as f64 / column_bytes as f64
            }
        };
        println!(
            "  {table}: {} of {} column bytes kept, {:.3}% against a budget of {BUDGET_SHARE}%",
            thousands(kept as u64),
            thousands(column_bytes),
            share(kept),
        );
        println!(
            "  {table}: {} built, {:.3}% if every column were kept, in {:.2}s",
            thousands(measured as u64),
            share(measured),
            spent.as_secs_f64(),
        );
    }
    Ok(())
}

/// One row, which is one column's statistics.
fn report(table: &str, column: &str, built: &Built) {
    println!(
        "  {:<12} {:<18} {:>13} {:>12} {:>7} {:>9} {:>8} {:>7} {:>7.2}s  {}",
        table,
        column,
        thousands(built.rows),
        thousands(built.distinct),
        if built.exact { "yes" } else { "no" },
        thousands(built.summary_bytes as u64),
        thousands(built.sketch_bytes as u64),
        built.stripes,
        built.build.as_secs_f64(),
        if built.built { "yes" } else { "no, over budget" },
    );
}

/// A number with separators, since these run to ten digits and are read by eye.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
