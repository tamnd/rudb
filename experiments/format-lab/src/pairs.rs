//! The pairwise pass.
//!
//! Section 6.4 wants to know which columns overlap enough to be worth one dictionary, and section
//! 6.6 wants to know which columns are determined by another column and therefore do not have to be
//! stored at all. Both questions are about pairs, a 105 column table has 5,460 of them, and neither
//! can be answered by looking at one column at a time.
//!
//! The trick that makes it affordable is that a pair is two hashes. Each column is hashed once a
//! row, and the pair hash of two columns is those two hashes combined, so testing all 5,460 pairs
//! costs one multiply and one compare per pair per row rather than a pass over the data per pair.
//! The whole thing then rides on the same single walk over the file as everything else.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rudb_common::{Error, Result};
use rudb_encoding::sketch::{self, Sketch};

use crate::column::{self, Column};
use crate::ingest::Source;
use crate::mem;
use crate::text::{self, Table};

/// What the run was asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    pub chunk_rows: usize,
    pub limit: Option<usize>,
    /// Sketch size. Smaller than the single column default because there are 5,460 of these and
    /// the questions they answer are yes or no rather than how many.
    pub k: usize,
    pub threads: usize,
    pub markdown: bool,
    pub columns: Vec<String>,
    /// How many rows of each table to print.
    pub top: usize,
    pub dependence_min: f64,
    pub jaccard_min: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_rows: 122_880,
            limit: None,
            k: 1024,
            threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
            markdown: false,
            columns: Vec::new(),
            top: 40,
            dependence_min: 0.98,
            jaccard_min: 0.05,
        }
    }
}

/// One column's hashes for the current chunk, and its sketch for the whole run.
#[derive(Debug)]
struct Work {
    name: String,
    kind: String,
    strings: bool,
    hashes: Vec<u64>,
    sketch: Sketch,
    /// The same sketch with the empty value left out, for the overlap question only.
    ///
    /// This lab stores a null as an empty string, and on `hits` most of the string columns are
    /// mostly null, so `sketch` says that sixteen columns which share nothing at all overlap. The
    /// dependency question wants the nulls counted, because a column that is always null really is
    /// determined by everything and the point is to notice that. The overlap question does not,
    /// because a shared dictionary is about shared vocabulary and the empty string is not any.
    without_empty: Sketch,
}

pub fn run(path: &Path, options: &Options) -> Result<()> {
    let source = Source::open(path)?;
    let wanted = source.select(&options.columns)?;
    // A column with no encoding has no hashes either, and a pair involving one would be a pair of
    // one real column and a column of zeros.
    let fields: Vec<usize> = wanted
        .iter()
        .copied()
        .filter(|&index| {
            !matches!(Column::for_type(source.schema.field(index).data_type()), Column::Skipped)
        })
        .collect();
    if fields.len() < 2 {
        return Err(Error::invalid_input("a pairwise pass wants at least two columns"));
    }

    let mut work: Vec<Work> = Vec::with_capacity(fields.len());
    for &index in &fields {
        let field = source.schema.field(index);
        work.push(Work {
            name: field.name().clone(),
            kind: column::short_name(field.data_type()),
            strings: matches!(Column::for_type(field.data_type()), Column::Bytes(_)),
            hashes: Vec::new(),
            sketch: Sketch::new(options.k)?,
            without_empty: Sketch::new(options.k)?,
        });
    }

    let mut couples: Vec<(usize, usize)> = Vec::new();
    for left in 0..work.len() {
        for right in left + 1..work.len() {
            couples.push((left, right));
        }
    }
    let together: Vec<Mutex<Sketch>> = couples
        .iter()
        .map(|_| Sketch::new(options.k).map(Mutex::new))
        .collect::<Result<Vec<_>>>()?;

    println!(
        "{} is {} in {} row groups of {} rows",
        path.display(),
        text::bytes(source.file_bytes),
        source.row_groups,
        text::count(source.rows)
    );
    println!(
        "{} columns, {} pairs, sketch k {}, {} threads",
        work.len(),
        text::count(couples.len()),
        options.k,
        options.threads
    );
    if let Some(limit) = options.limit {
        println!("reading the first {} rows only", text::count(limit));
    }
    println!();

    let started = Instant::now();
    let mut chunks = 0usize;
    let mut rows = 0usize;
    source.stream(&fields, options.chunk_rows, options.limit, &mut |columns, held| {
        chunks += 1;
        rows += held;
        hash_all(columns, &mut work, options.threads);
        count_pairs(&work, &couples, &together, options.threads);
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

    let together: Vec<Sketch> = together
        .into_iter()
        .map(|cell| cell.into_inner().unwrap_or_else(|error| error.into_inner()))
        .collect();
    report(&work, &couples, &together, rows, options)?;
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

/// Hash every value of every column in this chunk, and feed the single column sketches.
///
/// The columns are handed out one at a time from a shared queue because URL is worth thirty of the
/// small integer columns and any fixed split of 105 columns across four threads leaves three of
/// them waiting.
fn hash_all(columns: &[Column], work: &mut [Work], threads: usize) {
    let queue: Mutex<Vec<(usize, &mut Work)>> = Mutex::new(work.iter_mut().enumerate().collect());
    let threads = threads.clamp(1, columns.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let next = {
                        let mut queue = queue.lock().unwrap_or_else(|error| error.into_inner());
                        queue.pop()
                    };
                    let Some((index, work)) = next else { return };
                    work.hashes.clear();
                    match &columns[index] {
                        Column::Bytes(bytes) => {
                            for value in bytes.values() {
                                let hash = sketch::hash64(value);
                                work.hashes.push(hash);
                                if !value.is_empty() {
                                    work.without_empty.add_hash(hash);
                                }
                            }
                        }
                        Column::Ints(ints) => {
                            for value in ints.values() {
                                work.hashes.push(sketch::hash64(&value.to_le_bytes()));
                            }
                        }
                        Column::Skipped => {}
                    }
                    for &hash in &work.hashes {
                        work.sketch.add_hash(hash);
                    }
                }
            });
        }
    });
}

/// Feed every pair sketch from the hashes of this chunk.
///
/// This is the expensive loop of the whole lab, 5,460 pairs by 122,880 rows a chunk, so it is two
/// multiplies and a compare and nothing else. Once a sketch is full `add_hash` rejects on one
/// comparison against its largest, which all but a few thousand values of a column take.
fn count_pairs(
    work: &[Work],
    couples: &[(usize, usize)],
    together: &[Mutex<Sketch>],
    threads: usize,
) {
    let next = AtomicUsize::new(0);
    let threads = threads.clamp(1, couples.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= couples.len() {
                        return;
                    }
                    let (left, right) = couples[index];
                    let left = &work[left].hashes;
                    let right = &work[right].hashes;
                    let mut sketch =
                        together[index].lock().unwrap_or_else(|error| error.into_inner());
                    for (left, right) in left.iter().zip(right.iter()) {
                        sketch.add_hash(sketch::pair_of(*left, *right));
                    }
                }
            });
        }
    });
}

fn report(
    work: &[Work],
    couples: &[(usize, usize)],
    together: &[Sketch],
    rows: usize,
    options: &Options,
) -> Result<()> {
    let distinct: Vec<f64> = work.iter().map(|work| work.sketch.distinct()).collect();

    // Two kinds of dependency are true and useless. A column with one value is determined by
    // everything, and a column with as many distinct values as there are rows determines
    // everything, so both ends of a reported rule have to be somewhere in between.
    let interesting = |side: usize| distinct[side] > 1.5 && distinct[side] < rows as f64 * 0.99;

    let mut rules: Vec<(usize, usize, f64)> = Vec::new();
    let mut overlaps: Vec<(usize, usize, f64)> = Vec::new();
    for (index, &(left, right)) in couples.iter().enumerate() {
        let pair = &together[index];
        let forward = sketch::dependence(&work[left].sketch, pair)?;
        let backward = sketch::dependence(&work[right].sketch, pair)?;
        if forward >= options.dependence_min && interesting(left) && interesting(right) {
            rules.push((left, right, forward));
        }
        if backward >= options.dependence_min && interesting(left) && interesting(right) {
            rules.push((right, left, backward));
        }
        if work[left].strings && work[right].strings {
            let overlap = work[left].without_empty.jaccard(&work[right].without_empty)?;
            if overlap >= options.jaccard_min {
                overlaps.push((left, right, overlap));
            }
        }
    }

    println!();
    println!("dependencies, where the left column determines the right one");
    if rules.is_empty() {
        println!("none at {:.2} or better", options.dependence_min);
    } else {
        // The interesting rules are the ones that save the most, which is the ones whose right hand
        // side has the most values to not store.
        rules.sort_by(|left, right| {
            distinct[right.1].total_cmp(&distinct[left.1]).then(right.2.total_cmp(&left.2))
        });
        let mut table = Table::new(&["determines", "type", "column", "type", "distinct", "score"]);
        for (left, right, score) in rules.iter().take(options.top) {
            table.row(&[
                work[*left].name.clone(),
                work[*left].kind.clone(),
                work[*right].name.clone(),
                work[*right].kind.clone(),
                text::count(distinct[*right] as usize),
                format!("{score:.3}"),
            ]);
        }
        table.print(options.markdown);
        println!("{} of {} pairs", text::count(rules.len()), text::count(couples.len()));
    }

    println!();
    println!("overlap between string columns, which is what a shared dictionary is worth");
    if overlaps.is_empty() {
        println!("none at {:.2} or better", options.jaccard_min);
    } else {
        overlaps.sort_by(|left, right| right.2.total_cmp(&left.2));
        let mut table =
            Table::new(&["left", "distinct", "right", "distinct", "jaccard", "union saves"]);
        for (left, right, overlap) in overlaps.iter().take(options.top) {
            let union = work[*left].without_empty.union(&work[*right].without_empty)?.distinct();
            let apart =
                work[*left].without_empty.distinct() + work[*right].without_empty.distinct();
            table.row(&[
                work[*left].name.clone(),
                text::count(work[*left].without_empty.distinct() as usize),
                work[*right].name.clone(),
                text::count(work[*right].without_empty.distinct() as usize),
                format!("{overlap:.3}"),
                format!("{:.1}%", (apart - union) / apart * 100.0),
            ]);
        }
        table.print(options.markdown);
        println!("{} of {} pairs", text::count(overlaps.len()), text::count(couples.len()));
    }
    Ok(())
}
