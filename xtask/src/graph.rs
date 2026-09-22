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
//!
//! # And then a relationship after all
//!
//! A forward link is over a relationship, so the argument for one is a relationship:
//! `child.column->parent.column`, given after the columns. Section 3.8 makes this an order and not
//! a preference. A link is built by looking every child key up in the parent's key map, so the map
//! has to be in the file before the link is asked for, which is why the key maps of one run happen
//! before its links and why a link named without its parent's column reports that it found no map
//! rather than building one behind the caller's back.

use std::path::Path;
use std::time::Duration;

use rudb_native::Catalog;
use rudb_native::graph::{Built, BuiltLink, Edge, build_key_maps, build_links};

/// Builds a key map over every column named on the command line, then prints what each one cost.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let mut path: Option<&String> = None;
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut edges: Vec<(String, String, String, String)> = Vec::new();
    for arg in args {
        if path.is_none() {
            path = Some(arg);
            continue;
        }
        if let Some((child, parent)) = arg.split_once("->") {
            let (child, parent) = (named(child)?, named(parent)?);
            edges.push((child.0, child.1, parent.0, parent.1));
            continue;
        }
        let column = named(arg)?;
        wanted.push(column);
    }
    let Some(path) = path else {
        return Err("cargo xtask graph <file.db> <table.column>... [child.c->parent.c]...".into());
    };
    if wanted.is_empty() && edges.is_empty() {
        return Err("no column and no relationship named, so there is nothing to build".to_string());
    }
    let file = Path::new(path);
    let catalog = Catalog::open(file).map_err(|error| format!("{path}: {error}"))?;
    println!("{path}");
    if !wanted.is_empty() {
        println!(
            "  {:<12} {:<16} {:>12} {:>9} {:>11} {:>12} {:>6} {:>9}  kept",
            "table", "column", "rows", "form", "bytes", "column", "share", "build"
        );
    }
    let mut bytes = 0;
    let mut spent = Duration::ZERO;
    for (table, column) in &wanted {
        let at = column_of(&catalog, table, column)?;
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
    if !wanted.is_empty() {
        println!("  {} bytes kept, built in {:.2}s", thousands(bytes as u64), spent.as_secs_f64());
    }
    if edges.is_empty() {
        return Ok(());
    }

    let mut resolved = Vec::with_capacity(edges.len());
    for (child, child_column, parent, parent_column) in &edges {
        resolved.push(Edge {
            child: child.clone(),
            child_column: column_of(&catalog, child, child_column)?,
            parent: parent.clone(),
            parent_column: column_of(&catalog, parent, parent_column)?,
        });
    }
    drop(catalog);
    let links = build_links(file, &resolved).map_err(|error| format!("{path}: {error}"))?;
    println!();
    println!(
        "  {:<28} {:>12} {:>7} {:>9} {:>11} {:>14} {:>6} {:>9}  kept",
        "relationship", "children", "linked", "form", "bytes", "table", "share", "build"
    );
    let mut link_bytes = 0;
    let mut link_spent = Duration::ZERO;
    for one in &links {
        link(one);
        link_spent += one.build;
        if one.built {
            link_bytes += one.bytes;
        }
    }
    println!(
        "  {} link bytes kept, built in {:.2}s",
        thousands(link_bytes as u64),
        link_spent.as_secs_f64()
    );
    Ok(())
}

/// A `table.column` argument, split.
fn named(arg: &str) -> Result<(String, String), String> {
    let (table, column) =
        arg.split_once('.').ok_or_else(|| format!("{arg} is not table.column"))?;
    if table.is_empty() || column.is_empty() {
        return Err(format!("{arg} is not table.column"));
    }
    Ok((table.to_string(), column.to_string()))
}

/// Where a named column sits in a named table, which is what the graph layer addresses it by.
fn column_of(catalog: &Catalog, table: &str, column: &str) -> Result<usize, String> {
    let reader = catalog.table(table).map_err(|error| format!("{table}: {error}"))?;
    let at = reader
        .table()
        .fields()
        .iter()
        .position(|field| field.name.eq_ignore_ascii_case(column))
        .ok_or_else(|| format!("{table} has no column {column}"));
    drop(reader);
    at
}

/// One row, which is one relationship's forward link.
///
/// The share is against the child table's stored column bytes and not against the one column's,
/// because that is what the budget of section 3.7 is a share of and what the claim of section 9.1
/// is measured against. Reading down this column and adding is the C1 measurement.
fn link(built: &BuiltLink) {
    let share = if built.table_bytes == 0 {
        f64::INFINITY
    } else {
        100.0 * built.bytes as f64 / built.table_bytes as f64
    };
    let name = format!("{} -> {}", built.edge.child, built.edge.parent);
    println!(
        "  {:<28} {:>12} {:>6.1}% {:>9} {:>11} {:>14} {:>5.2}% {:>8.2}s  {}",
        name,
        thousands(built.children),
        if built.children == 0 {
            100.0
        } else {
            100.0 * built.linked as f64 / built.children as f64
        },
        built.form.map_or("none", |form| form.label()),
        thousands(built.bytes as u64),
        thousands(built.table_bytes),
        share,
        built.build.as_secs_f64(),
        built.note.as_deref().unwrap_or(if built.built { "yes" } else { "no" }),
    );
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
