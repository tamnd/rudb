//! What sharing and recomputation are worth in bytes.
//!
//! The pairwise pass says which columns overlap and which columns are determined by another one.
//! Both of those are counts of distinct values, and a count of distinct values is not a saving. This
//! is the pass that encodes it both ways and subtracts.
//!
//! Two questions, and they are the last two size boxes of M1. Section 6.4 asks what a group of
//! columns sharing one dictionary or one symbol table saves over encoding them apart. Section 6.6
//! asks what a column that is determined by another column costs when it is stored as a mapping off
//! the other column's dictionary rather than as itself.
//!
//! The decisions here are made per chunk and not once for the file. A row group is the unit a
//! writer encodes, so a row group is the unit that gets to choose, and a grouping that is right for
//! the file and wrong for this row group is not a grouping the writer would have used.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use rudb_common::{Error, Result};
use rudb_encoding::multi::{self, Strategy};
use rudb_encoding::sketch::Sketch;
use rudb_encoding::{integer, string};

use crate::column::Column;
use crate::ingest::Source;
use crate::mem;
use crate::text::{self, Table};

/// What the run was asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    pub chunk_rows: usize,
    pub limit: Option<usize>,
    pub threads: usize,
    pub markdown: bool,
    pub columns: Vec<String>,
    /// Columns go in the same group when their sketches overlap by at least this much.
    pub jaccard_min: f64,
    /// The dependencies to price, as pairs of column names. These come from the pairwise pass, and
    /// they are given rather than discovered because a rule is worth pricing only after a human has
    /// looked at it.
    pub rules: Vec<(String, String)>,
    /// Sketch size for the grouping decision, which is a yes or no per pair.
    pub k: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_rows: 122_880,
            limit: None,
            threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
            markdown: false,
            columns: Vec::new(),
            jaccard_min: 0.05,
            rules: Vec::new(),
            k: 1024,
        }
    }
}

/// Running totals for one group of columns.
#[derive(Debug, Default, Clone)]
struct GroupTotals {
    independent: usize,
    shared_table: usize,
    shared_dict: usize,
    chunks: usize,
    wins: [usize; 3],
}

/// Running totals for one dependency.
#[derive(Debug, Default, Clone)]
struct RuleTotals {
    /// The determined column encoded as itself, which is what it costs today.
    stored: usize,
    /// The mapping alone, which is what it costs when the reader can index it by the determining
    /// column's dictionary code.
    on_codes: usize,
    /// The mapping plus the keys, which is what it costs when it cannot.
    with_keys: usize,
    /// Rows whose key already had a different value, which is the dependency not holding.
    violations: usize,
    keys: usize,
}

pub fn run(path: &Path, options: &Options) -> Result<()> {
    let source = Source::open(path)?;
    let fields = source.select(&options.columns)?;
    let names: Vec<String> =
        fields.iter().map(|&index| source.schema.field(index).name().clone()).collect();

    // The rules name columns, and everything below works on positions in the projection.
    let mut rules: Vec<(usize, usize)> = Vec::new();
    for (left, right) in &options.rules {
        let left = position(&names, left)?;
        let right = position(&names, right)?;
        rules.push((left, right));
    }

    println!(
        "{} is {} in {} row groups of {} rows",
        path.display(),
        text::bytes(source.file_bytes),
        source.row_groups,
        text::count(source.rows)
    );
    println!(
        "grouping at jaccard {:.2}, {} rules to price, {} threads",
        options.jaccard_min,
        rules.len(),
        options.threads
    );
    if let Some(limit) = options.limit {
        println!("reading the first {} rows only", text::count(limit));
    }
    println!();

    let started = Instant::now();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut totals: Vec<GroupTotals> = Vec::new();
    let mut rule_totals: Vec<RuleTotals> = vec![RuleTotals::default(); rules.len()];
    let mut chunks = 0usize;
    let mut rows = 0usize;

    source.stream(&fields, options.chunk_rows, options.limit, &mut |columns, held| {
        chunks += 1;
        rows += held;
        if groups.is_empty() {
            groups = choose_groups(columns, options)?;
            totals = vec![GroupTotals::default(); groups.len()];
            print_groups(&groups, &names);
        }
        for (slot, group) in groups.iter().enumerate() {
            price_group(columns, group, &mut totals[slot])?;
        }
        for (slot, rule) in rules.iter().enumerate() {
            price_rule(columns, *rule, &mut rule_totals[slot])?;
        }
        println!("  {} rows in {:.0}s", text::count(rows), started.elapsed().as_secs_f64());
        Ok(())
    })?;

    report(&groups, &totals, &rules, &rule_totals, &names, options);
    println!();
    println!(
        "{} rows in {} chunks in {:.1}s wall clock",
        text::count(rows),
        chunks,
        started.elapsed().as_secs_f64()
    );
    if let Some(peak) = mem::peak_rss() {
        println!("peak resident {}", text::bytes(peak));
    }
    Ok(())
}

fn position(names: &[String], name: &str) -> Result<usize> {
    names
        .iter()
        .position(|other| other == name)
        .ok_or_else(|| Error::invalid_input(format!("no column named {name}")))
}

/// Sketch the string columns of this chunk and let `dictionary_groups` do the union find.
///
/// Only string columns, because a shared dictionary over an integer column and a string column is
/// not a thing either shape in `multi` can express.
fn choose_groups(columns: &[Column], options: &Options) -> Result<Vec<Vec<usize>>> {
    let mut sketches: Vec<Sketch> = Vec::with_capacity(columns.len());
    for column in columns {
        let mut sketch = Sketch::new(options.k)?;
        if let Column::Bytes(bytes) = column {
            for value in bytes.values() {
                sketch.add(value);
            }
        }
        sketches.push(sketch);
    }
    let groups = multi::dictionary_groups(&sketches, options.jaccard_min)?;
    // A group of one is every column that shares with nothing, and there is nothing to measure
    // about it. A group of strings only, because an empty sketch overlaps nothing anyway.
    Ok(groups.into_iter().filter(|group| group.len() > 1).collect())
}

fn print_groups(groups: &[Vec<usize>], names: &[String]) {
    if groups.is_empty() {
        println!("no two columns overlap enough to share, so there is nothing to price");
        return;
    }
    println!("groups from the first chunk:");
    for group in groups {
        let members: Vec<&str> = group.iter().map(|&index| names[index].as_str()).collect();
        println!("  {}", members.join(", "));
    }
    println!();
}

fn price_group(columns: &[Column], group: &[usize], totals: &mut GroupTotals) -> Result<()> {
    let held: Vec<Vec<&[u8]>> = group
        .iter()
        .map(|&index| match &columns[index] {
            Column::Bytes(bytes) => bytes.values(),
            _ => Vec::new(),
        })
        .collect();
    let borrowed: Vec<&[&[u8]]> = held.iter().map(|values| values.as_slice()).collect();
    let sizes = multi::strategy_sizes(&borrowed)?;

    let mut best = (Strategy::Independent, usize::MAX);
    for (strategy, size) in &sizes {
        match strategy {
            Strategy::Independent => totals.independent += size,
            Strategy::SharedTable => totals.shared_table += size,
            Strategy::SharedDict => totals.shared_dict += size,
        }
        if *size < best.1 {
            best = (*strategy, *size);
        }
    }
    totals.wins[best.0 as usize] += 1;
    totals.chunks += 1;
    Ok(())
}

/// Price one dependency over one chunk.
///
/// The mapping is built in first appearance order of the determining column, which is the order its
/// dictionary would be in, so the values come out in the order the reader would index them.
fn price_rule(columns: &[Column], rule: (usize, usize), totals: &mut RuleTotals) -> Result<()> {
    let (left, right) = rule;
    let keys = values_of(&columns[left]);
    let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut order: Vec<Vec<u8>> = Vec::new();
    let mut mapped_bytes: Vec<Vec<u8>> = Vec::new();
    let mut mapped_ints: Vec<i64> = Vec::new();

    let strings = matches!(&columns[right], Column::Bytes(_));
    let right_bytes: Vec<&[u8]> = match &columns[right] {
        Column::Bytes(bytes) => bytes.values(),
        _ => Vec::new(),
    };
    let right_ints: &[i64] = match &columns[right] {
        Column::Ints(ints) => ints.values(),
        _ => &[],
    };

    for (row, key) in keys.iter().enumerate() {
        match seen.get(key.as_slice()) {
            Some(&slot) => {
                let same = if strings {
                    mapped_bytes[slot].as_slice() == right_bytes[row]
                } else {
                    mapped_ints[slot] == right_ints[row]
                };
                if !same {
                    totals.violations += 1;
                }
            }
            None => {
                seen.insert(key.clone(), order.len());
                order.push(key.clone());
                if strings {
                    mapped_bytes.push(right_bytes[row].to_vec());
                } else {
                    mapped_ints.push(right_ints[row]);
                }
            }
        }
    }

    let stored = if strings {
        string::encode(&right_bytes)?.len()
    } else {
        integer::encode(right_ints)?.len()
    };
    let on_codes = if strings {
        let view: Vec<&[u8]> = mapped_bytes.iter().map(|value| value.as_slice()).collect();
        string::encode(&view)?.len()
    } else {
        integer::encode(&mapped_ints)?.len()
    };
    let key_view: Vec<&[u8]> = order.iter().map(|value| value.as_slice()).collect();
    let key_size = string::encode(&key_view)?.len();

    totals.stored += stored;
    totals.on_codes += on_codes;
    totals.with_keys += on_codes + key_size;
    totals.keys += order.len();
    Ok(())
}

/// The values of a column as byte strings, which is what a hash map key has to be for a column
/// whose type is not known until run time. An integer becomes its eight little endian bytes.
fn values_of(column: &Column) -> Vec<Vec<u8>> {
    match column {
        Column::Bytes(bytes) => bytes.values().into_iter().map(|value| value.to_vec()).collect(),
        Column::Ints(ints) => {
            ints.values().iter().map(|value| value.to_le_bytes().to_vec()).collect()
        }
        Column::Skipped => Vec::new(),
    }
}

fn report(
    groups: &[Vec<usize>],
    totals: &[GroupTotals],
    rules: &[(usize, usize)],
    rule_totals: &[RuleTotals],
    names: &[String],
    options: &Options,
) {
    if !groups.is_empty() {
        println!();
        println!("what sharing is worth, summed over every chunk");
        let mut table =
            Table::new(&["group", "apart", "shared table", "shared dict", "best saves", "picked"]);
        for (group, totals) in groups.iter().zip(totals.iter()) {
            let members: Vec<&str> = group.iter().map(|&index| names[index].as_str()).collect();
            let best = totals.shared_table.min(totals.shared_dict).min(totals.independent);
            let picked = ["apart", "shared table", "shared dict"]
                .iter()
                .enumerate()
                .filter(|(slot, _)| totals.wins[*slot] > 0)
                .map(|(slot, name)| format!("{name} {}", totals.wins[slot]))
                .collect::<Vec<_>>()
                .join(", ");
            table.row(&[
                members.join(" + "),
                text::bytes(totals.independent),
                text::bytes(totals.shared_table),
                text::bytes(totals.shared_dict),
                format!(
                    "{:.1}%",
                    (totals.independent as f64 - best as f64) / totals.independent.max(1) as f64
                        * 100.0
                ),
                picked,
            ]);
        }
        table.print(options.markdown);
    }

    if !rules.is_empty() {
        println!();
        println!("what a recomputation rule is worth, summed over every chunk");
        let mut table = Table::new(&[
            "determines",
            "column",
            "stored",
            "rule on codes",
            "rule with keys",
            "saves",
            "keys",
            "violations",
        ]);
        for ((left, right), totals) in rules.iter().zip(rule_totals.iter()) {
            table.row(&[
                names[*left].clone(),
                names[*right].clone(),
                text::bytes(totals.stored),
                text::bytes(totals.on_codes),
                text::bytes(totals.with_keys),
                format!(
                    "{:.1}%",
                    (totals.stored as f64 - totals.on_codes as f64) / totals.stored.max(1) as f64
                        * 100.0
                ),
                text::count(totals.keys),
                text::count(totals.violations),
            ]);
        }
        table.print(options.markdown);
        println!();
        println!(
            "a rule with any violations at all is not a rule, and the column has to be stored"
        );
    }
}
