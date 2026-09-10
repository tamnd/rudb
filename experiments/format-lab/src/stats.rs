//! The single column pass.
//!
//! One walk over the file. Per chunk and per column it runs the chooser, records what it picked and
//! what it cost, decodes it again and checks it came back the same, and feeds every value to a
//! sketch. That is boxes one, two, four and seven of the milestone in one pass, and the round trip
//! is the part that is not in the milestone but is the reason to run it on real data at all: the
//! encoders have only ever seen values this file's URL column would laugh at.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rudb_common::{Error, Result};
use rudb_encoding::sketch::{self, Sketch};
use rudb_encoding::{integer, string};

use crate::column::{self, Column};
use crate::ingest::Source;
use crate::mem;
use crate::text::{self, Table};

/// What the run was asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    pub chunk_rows: usize,
    pub limit: Option<usize>,
    pub sketch_k: usize,
    pub verify: bool,
    pub markdown: bool,
    pub threads: usize,
    pub columns: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // DuckDB's row group. See the comment in ingest.rs for why it is not a rounder number.
            chunk_rows: 122_880,
            limit: None,
            sketch_k: sketch::DEFAULT_K,
            verify: true,
            markdown: false,
            threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
            columns: Vec::new(),
        }
    }
}

/// Everything the run learned about one column.
#[derive(Debug)]
struct ColumnStats {
    name: String,
    kind: String,
    rows: usize,
    /// Bytes as the lab counts the values, which is the characters for a string column and eight a
    /// value for anything fixed width.
    logical: usize,
    encoded: usize,
    sketch: Sketch,
    /// Chunk counts per top level shape, in the order first seen.
    shapes: Vec<(String, usize)>,
    /// The whole shape of the first chunk, brackets and all. The table has room for the head of it
    /// and the head is what says which shape won, but the head is not what says why a column came
    /// out the size it did. `DICT` on a column of nineteen million URLs is the same word whether
    /// the dictionary went back through the chooser and came out FSST or whether it is sitting
    /// there as plain bytes, and those two are a gigabyte apart.
    first_shape: String,
    encode_nanos: u128,
    decode_nanos: u128,
    mismatches: usize,
    skipped: bool,
}

impl ColumnStats {
    fn new(name: String, kind: String, k: usize, skipped: bool) -> Result<Self> {
        Ok(Self {
            name,
            kind,
            rows: 0,
            logical: 0,
            encoded: 0,
            sketch: Sketch::new(k)?,
            shapes: Vec::new(),
            first_shape: String::new(),
            encode_nanos: 0,
            decode_nanos: 0,
            mismatches: 0,
            skipped,
        })
    }

    fn saw(&mut self, shape: &str) {
        if self.first_shape.is_empty() {
            self.first_shape = shape.to_string();
        }
        let head = head_of(shape);
        match self.shapes.iter_mut().find(|(name, _)| name == &head) {
            Some((_, count)) => *count += 1,
            None => self.shapes.push((head, 1)),
        }
    }

    fn shape_text(&self) -> String {
        if self.shapes.is_empty() {
            return "-".to_string();
        }
        let mut shapes = self.shapes.clone();
        shapes.sort_by_key(|shape| std::cmp::Reverse(shape.1));
        shapes.iter().map(|(name, count)| format!("{name} {count}")).collect::<Vec<_>>().join(" ")
    }
}

/// `FSST[41](PLAIN(FOR+BITPACK[7]))` is more detail than a table column can hold, and the question
/// the table answers is which shape won, so everything from the first bracket is dropped.
fn head_of(shape: &str) -> String {
    let end = shape.find(['(', '[']).unwrap_or(shape.len());
    shape[..end].to_string()
}

pub fn run(path: &Path, options: &Options) -> Result<()> {
    let source = Source::open(path)?;
    let fields = source.select(&options.columns)?;
    let started = Instant::now();

    println!(
        "{} is {} in {} row groups of {} rows, {} columns, codec {}",
        path.display(),
        text::bytes(source.file_bytes),
        source.row_groups,
        text::count(source.rows),
        source.schema.fields().len(),
        source.codecs
    );
    if let Some(limit) = options.limit {
        println!("reading the first {} rows only", text::count(limit));
    }
    println!(
        "chunk {} rows, sketch k {}, {} threads, verify {}",
        text::count(options.chunk_rows),
        options.sketch_k,
        options.threads,
        if options.verify { "on" } else { "off" }
    );
    println!();

    let cells: Vec<Mutex<ColumnStats>> = fields
        .iter()
        .map(|&index| {
            let field = source.schema.field(index);
            let kind = column::short_name(field.data_type());
            let skipped = matches!(Column::for_type(field.data_type()), Column::Skipped);
            ColumnStats::new(field.name().clone(), kind, options.sketch_k, skipped).map(Mutex::new)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut chunks = 0usize;
    let mut rows = 0usize;
    let losses =
        source.stream(&fields, options.chunk_rows, options.limit, &mut |columns, held| {
            chunks += 1;
            rows += held;
            measure(columns, held, &cells, options)?;
            if chunks.is_multiple_of(10) {
                let elapsed = started.elapsed().as_secs_f64();
                println!(
                    "  {} rows in {:.0}s, {} rows/s",
                    text::count(rows),
                    elapsed,
                    text::count((rows as f64 / elapsed) as usize)
                );
            }
            Ok(())
        })?;

    let elapsed = started.elapsed();
    let stats: Vec<ColumnStats> = cells
        .into_iter()
        .map(|cell| cell.into_inner().unwrap_or_else(|error| error.into_inner()))
        .collect();

    report(&source, &fields, &stats, &losses, options);
    println!();
    println!(
        "{} rows in {} chunks in {:.1}s wall clock",
        text::count(rows),
        chunks,
        elapsed.as_secs_f64()
    );
    match (mem::peak_rss(), mem::rss()) {
        (Some(peak), Some(now)) => {
            println!("peak resident {}, resident now {}", text::bytes(peak), text::bytes(now))
        }
        _ => println!("resident size is not readable on this platform"),
    }
    Ok(())
}

/// One chunk, spread over the worker threads by column.
///
/// The split is by column and not by chunk because a chunk is the memory bound: two chunks live at
/// once would double the peak, and the peak is the number the milestone asks for. Columns within a
/// chunk are wildly uneven, URL is worth thirty of the small integer columns, so the work is handed
/// out one column at a time from a shared counter rather than sliced up front.
fn measure(
    columns: &[Column],
    held: usize,
    cells: &[Mutex<ColumnStats>],
    options: &Options,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<Error>> = Mutex::new(None);
    let threads = options.threads.clamp(1, columns.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= columns.len() {
                        return;
                    }
                    let mut stats = cells[index].lock().unwrap_or_else(|error| error.into_inner());
                    // Counted here rather than in `one` so that a column whose type has no
                    // encoding still knows how many rows went past it, which is what the Parquet
                    // side of its row is scaled by.
                    stats.rows += held;
                    if let Err(error) = one(&columns[index], &mut stats, options) {
                        let mut slot = failed.lock().unwrap_or_else(|error| error.into_inner());
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                        return;
                    }
                }
            });
        }
    });
    match failed.into_inner().unwrap_or_else(|error| error.into_inner()) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn one(column: &Column, stats: &mut ColumnStats, options: &Options) -> Result<()> {
    stats.logical += column.bytes();
    match column {
        Column::Skipped => Ok(()),
        Column::Bytes(bytes) => {
            let values = bytes.values();
            for value in &values {
                stats.sketch.add(value);
            }
            let at = Instant::now();
            let encoded = string::encode(&values)?;
            stats.encode_nanos += at.elapsed().as_nanos();
            stats.encoded += encoded.len();
            stats.saw(&string::describe(&encoded)?);
            if options.verify {
                let at = Instant::now();
                let back = string::decode(&encoded)?;
                stats.decode_nanos += at.elapsed().as_nanos();
                if back.len() != values.len() {
                    stats.mismatches += values.len().abs_diff(back.len());
                }
                for (left, right) in values.iter().zip(back.iter()) {
                    if *left != right.as_slice() {
                        stats.mismatches += 1;
                    }
                }
            }
            Ok(())
        }
        Column::Ints(ints) => {
            let values = ints.values();
            for value in values {
                stats.sketch.add_hash(sketch::hash64(&value.to_le_bytes()));
            }
            let at = Instant::now();
            let encoded = integer::encode(values)?;
            stats.encode_nanos += at.elapsed().as_nanos();
            stats.encoded += encoded.len();
            stats.saw(&integer::describe(&encoded)?);
            if options.verify {
                let at = Instant::now();
                let back = integer::decode(&encoded)?;
                stats.decode_nanos += at.elapsed().as_nanos();
                if back != values {
                    stats.mismatches += back
                        .iter()
                        .zip(values.iter())
                        .filter(|(left, right)| left != right)
                        .count()
                        .max(back.len().abs_diff(values.len()));
                }
            }
            Ok(())
        }
    }
}

fn report(
    source: &Source,
    fields: &[usize],
    stats: &[ColumnStats],
    losses: &[column::Losses],
    options: &Options,
) {
    let mut table = Table::new(&[
        "column", "type", "distinct", "nulls", "parquet", "rudb", "rudb/pq", "b/row", "shape",
    ]);
    let mut total_parquet = 0usize;
    let mut total_rudb = 0usize;
    let mut total_logical = 0usize;
    let mut encode_nanos = 0u128;
    let mut decode_nanos = 0u128;
    let mut mismatches = 0usize;
    let mut narrowed = 0usize;

    for (slot, stats) in stats.iter().enumerate() {
        let parquet = source.parquet_bytes.get(fields[slot]).copied().unwrap_or_default();
        // A partial read is being compared against a whole file, so the Parquet side is scaled to
        // the rows actually read. It is the best that can be done and it is only exact when the
        // rows read are representative, which is why the default is to read all of them.
        let parquet = if source.rows > 0 && stats.rows < source.rows {
            parquet * stats.rows / source.rows
        } else {
            parquet
        };
        if stats.skipped {
            table.row(&[
                stats.name.clone(),
                stats.kind.clone(),
                "-".into(),
                "-".into(),
                text::bytes(parquet),
                "skipped".into(),
                "-".into(),
                "-".into(),
                "no encoding yet".into(),
            ]);
            continue;
        }
        total_parquet += parquet;
        total_rudb += stats.encoded;
        total_logical += stats.logical;
        encode_nanos += stats.encode_nanos;
        decode_nanos += stats.decode_nanos;
        mismatches += stats.mismatches;
        narrowed += losses[slot].narrowed;

        let distinct = if stats.sketch.is_exact() {
            text::count(stats.sketch.len())
        } else {
            format!("~{}", text::count(stats.sketch.distinct() as usize))
        };
        table.row(&[
            stats.name.clone(),
            stats.kind.clone(),
            distinct,
            text::count(losses[slot].nulls),
            text::bytes(parquet),
            text::bytes(stats.encoded),
            text::ratio(stats.encoded, parquet),
            format!("{:.2}", stats.encoded as f64 / stats.rows.max(1) as f64),
            stats.shape_text(),
        ]);
    }

    table.row(&[
        "TOTAL".into(),
        String::new(),
        String::new(),
        String::new(),
        text::bytes(total_parquet),
        text::bytes(total_rudb),
        text::ratio(total_rudb, total_parquet),
        format!(
            "{:.2}",
            total_rudb as f64 / stats.first().map_or(1, |first| first.rows.max(1)) as f64
        ),
        String::new(),
    ]);
    table.print(options.markdown);

    println!();
    println!("the whole shape of the first chunk, for the columns that carry the file");
    let mut biggest: Vec<&ColumnStats> =
        stats.iter().filter(|stats| !stats.skipped && stats.encoded > 0).collect();
    biggest.sort_by_key(|stats| std::cmp::Reverse(stats.encoded));
    for stats in biggest.iter().take(12) {
        println!("  {:22} {}", stats.name, stats.first_shape);
    }

    println!();
    println!(
        "values as read {}, parquet {}, rudb {}",
        text::bytes(total_logical),
        text::bytes(total_parquet),
        text::bytes(total_rudb)
    );
    if encode_nanos > 0 {
        println!(
            "encode {:.0} MB/s of values, cpu time {:.0}s",
            total_logical as f64 / (encode_nanos as f64 / 1e9) / 1e6,
            encode_nanos as f64 / 1e9
        );
    }
    if decode_nanos > 0 {
        println!(
            "decode {:.0} MB/s of values, cpu time {:.0}s",
            total_logical as f64 / (decode_nanos as f64 / 1e9) / 1e6,
            decode_nanos as f64 / 1e9
        );
    }
    if options.verify {
        if mismatches == 0 {
            println!("every chunk decoded back to what went in");
        } else {
            println!("{} values did not come back", text::count(mismatches));
        }
    }
    if narrowed > 0 {
        println!(
            "{} values did not fit in an i64 and were clamped, sizes for those columns are not real",
            text::count(narrowed)
        );
    }
}
