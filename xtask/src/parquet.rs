//! What reading a Parquet file costs in rudb next to what it costs in DuckDB.
//!
//! The goal is ten times faster than DuckDB and the first thing that needs is a number saying how
//! far away that is on the one path both engines already share. Neither engine's own format is
//! involved here. Both are pointed at the same Parquet file and asked the same SQL, so what the
//! table measures is the reader, the decoder and whatever runs on top of the values, with the
//! storage format held fixed.
//!
//! [`crate::differential`] already runs two engines over one file and it is not this. It runs every
//! ClickBench query once each and its question is whether the answers match. One run is enough to
//! see a twenty times gap and is not enough to see a ten percent one, and it says so. This one runs
//! a handful of shapes many times each and its question is how long they take.
//!
//! # How a number here is made
//!
//! Both engines are measured the same way, as a subprocess running a script of SQL, because the
//! alternative is linking one of them and shelling out to the other and then arguing about which
//! side the difference came from. rudb's shell takes the same flags DuckDB's does, so the two
//! commands differ only in the binary at the front of them.
//!
//! Process start is subtracted rather than included. Each engine first runs an empty script, which
//! is everything it does other than the query, and that is taken off the wall clock of a script
//! holding the query many times over. The repeat count is calibrated so the queries are at least
//! [`TARGET_NANOS`] of the total, which puts the uncertainty in the start time below one percent of
//! what is left. Dividing by the repeat count gives the per query cost with the process gone.
//!
//! Every repeat after the first reads a file the operating system has already cached, and DuckDB
//! caches the bytes again inside itself. So these are warm numbers on both sides. That is the right
//! comparison for an engine people run queries in and it is the wrong one for a cold first touch,
//! and nothing here measures the cold one.
//!
//! # Why these shapes
//!
//! Each one adds a stage to the one above it, so the difference between two rows says what the
//! stage between them cost rather than what a whole query cost.
//!
//! `count` reads the footer and no column at all, so it is the fixed cost of opening the file.
//! `int1` adds one eight byte integer column, `int10` adds nine more of assorted widths, `str1`
//! adds one large string column instead, `str4` adds the four the file is mostly made of, and
//! `filter` puts a predicate under the aggregate so the rows that survive are a fraction rather
//! than all of them. `load` is every column of every row into a table, which is the shape a loader
//! has and the shape F2's first exit criterion is about.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::compare::{binary, build_rudb};
use crate::timing::{Number, SAMPLES, build_line, percentile, show};

/// How much of a calibrated script has to be the query rather than the process around it.
///
/// Four hundred milliseconds against a process start of a few tens of milliseconds, whose run to
/// run spread is a few milliseconds. The start is measured separately and subtracted, so what its
/// spread does to the result is that spread divided by this, which is well under one percent.
const TARGET_NANOS: f64 = 400_000_000.0;

/// A ceiling on the repeat count, so a query that takes microseconds cannot ask for a script that
/// takes longer to write than to run.
const MAX_REPEATS: usize = 20_000;

/// One thing to ask both engines, and what it adds to the thing above it.
struct Shape {
    /// What the table calls it.
    name: &'static str,
    /// The SQL, with `{}` where the file goes.
    sql: &'static str,
    /// What it costs that the row above it did not.
    adds: &'static str,
}

/// The ladder, in the order it is printed, each rung one stage taller than the last.
const SHAPES: &[Shape] = &[
    Shape { name: "count", sql: "SELECT count(*) FROM {}", adds: "the footer, and no column" },
    Shape {
        name: "int1",
        sql: "SELECT sum(UserID) FROM {}",
        adds: "one eight byte integer column",
    },
    Shape {
        name: "int10",
        sql: "SELECT sum(WatchID), sum(JavaEnable), sum(GoodEvent), sum(ClientIP), sum(RegionID), \
              sum(UserID), sum(CounterClass), sum(OS), sum(UserAgent), sum(ResolutionWidth) FROM {}",
        adds: "nine more integer columns, of four widths",
    },
    Shape {
        name: "str1",
        sql: "SELECT sum(length(URL)) FROM {}",
        adds: "one large string column instead of the integers",
    },
    Shape {
        name: "str4",
        sql: "SELECT sum(length(URL)), sum(length(Title)), sum(length(Referer)), \
              sum(length(OriginalURL)) FROM {}",
        adds: "the four string columns the file is mostly made of",
    },
    Shape {
        name: "filter",
        sql: "SELECT count(*) FROM {} WHERE CounterID = 62",
        adds: "a predicate, so the rows kept are a fraction of the rows read",
    },
    Shape {
        name: "load",
        sql: "CREATE TABLE loaded AS SELECT * FROM {}",
        adds: "every column of every row, into a table",
    },
];

/// What one engine did with one shape.
struct Cell {
    /// Nanoseconds a query, the median of [`SAMPLES`] samples with the process start taken off.
    took: Number,
    /// How many copies of the query were in each script.
    repeats: usize,
}

/// Measure both engines over each file named in `args` and print a table for each.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let mut files = Vec::new();
    let mut threads = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--threads" => {
                let Some(value) = rest.next() else {
                    return Err("--threads wants a number after it".into());
                };
                threads = Some(
                    value.parse::<usize>().map_err(|_| format!("{value} is not a thread count"))?,
                );
            }
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        files.push(root.join("crates").join("rudb").join("testdata").join("hits.parquet"));
    }

    let rudb = match std::env::var_os("RUDB_BENCH_RUDB") {
        Some(set) => PathBuf::from(set),
        None => build_rudb(root)?,
    };
    if !rudb.is_file() {
        return Err(format!("no rudb at {}", rudb.display()));
    }
    let duckdb = duckdb()?;
    let scratch = root.join("target").join("parquet-read");
    std::fs::create_dir_all(&scratch)
        .map_err(|e| format!("could not make {}: {e}", scratch.display()))?;

    println!();
    println!("what a Parquet read costs, rudb against duckdb");
    println!("  {}", build_line());
    println!("  rudb    {}", rudb.display());
    println!("  duckdb  {}", duckdb.display());
    match threads {
        Some(count) => println!("  both engines set to {count} threads"),
        None => println!("  both engines left at their own default thread count"),
    }
    println!("  {SAMPLES} samples a cell, process start measured separately and subtracted");

    let ours = start(&rudb, &scratch, threads, "rudb")?;
    let theirs = start(&duckdb, &scratch, threads, "duckdb")?;
    println!(
        "  process start is {} for rudb and {} for duckdb",
        show(ours.median),
        show(theirs.median)
    );

    for file in &files {
        let file = file
            .canonicalize()
            .map_err(|e| format!("could not resolve {}: {e}", file.display()))?;
        if !file.is_file() {
            return Err(format!("{} is not a file", file.display()));
        }
        table(&file, &rudb, &duckdb, &scratch, threads, ours, theirs)?;
    }

    println!();
    println!("caveats");
    for line in caveats() {
        println!("{line}");
    }
    Ok(())
}

/// One file's worth of rows.
fn table(
    file: &Path,
    rudb: &Path,
    duckdb: &Path,
    scratch: &Path,
    threads: Option<usize>,
    ours_start: Number,
    theirs_start: Number,
) -> Result<(), String> {
    let bytes = std::fs::metadata(file).map(|meta| meta.len()).unwrap_or(0);
    println!();
    println!("{}, {:.2} MiB on disk", file.display(), bytes as f64 / (1024.0 * 1024.0));
    println!(
        "  {:<7} {:>14} {:>14} {:>9} {:>11}  what it adds",
        "shape", "rudb", "duckdb", "rudb/ddb", "repeats"
    );

    for shape in SHAPES {
        let sql = shape.sql.replace("{}", &format!("'{}'", file.display()));
        let ours = cell(rudb, scratch, threads, &sql, ours_start, "rudb")?;
        let theirs = cell(duckdb, scratch, threads, &sql, theirs_start, "duckdb")?;
        let ratio =
            if theirs.took.median > 0.0 { ours.took.median / theirs.took.median } else { 0.0 };
        println!(
            "  {:<7} {:>14} {:>14} {:>9} {:>11}  {}",
            shape.name,
            spread(ours.took),
            spread(theirs.took),
            format!("{ratio:.2}x"),
            format!("{}/{}", ours.repeats, theirs.repeats),
            shape.adds
        );
    }
    Ok(())
}

/// A median with its spread, in the form the table prints.
fn spread(number: Number) -> String {
    format!("{} {:.0}%", show(number.median), number.relative() * 100.0)
}

/// What the engine costs before it has been asked anything.
///
/// An empty script rather than a trivial query, because a trivial query is a parse and a plan and
/// those are per query costs that belong in the cell rather than in what is subtracted from it.
fn start(
    engine: &Path,
    scratch: &Path,
    threads: Option<usize>,
    name: &str,
) -> Result<Number, String> {
    let script = write(scratch, &format!("{name}-start.sql"), "")?;
    let mut samples = Vec::with_capacity(SAMPLES);
    // One discarded run first, so the page cache holds the binary and its libraries before the
    // samples begin. Otherwise the first sample carries a load that no later one does and the
    // number that gets subtracted is too large.
    went(engine, &script, threads)?;
    for _ in 0..SAMPLES {
        samples.push(went(engine, &script, threads)?.as_nanos() as f64);
    }
    samples.sort_by(f64::total_cmp);
    Ok(Number {
        median: percentile(&samples, 50),
        iqr: percentile(&samples, 75) - percentile(&samples, 25),
    })
}

/// One engine on one shape, with the process taken back out of it.
fn cell(
    engine: &Path,
    scratch: &Path,
    threads: Option<usize>,
    sql: &str,
    overhead: Number,
    name: &str,
) -> Result<Cell, String> {
    let repeats = calibrate(engine, scratch, threads, sql, overhead, name)?;
    let script = repeated(scratch, name, sql, repeats)?;
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let whole = went(engine, &script, threads)?.as_nanos() as f64;
        if std::env::var_os("RUDB_PARQUET_TRACE").is_some() {
            // On standard error rather than standard output, so turning it on does not put lines
            // through the middle of the table that everything else here exists to print.
            eprintln!("trace {name} repeats {repeats} whole {}", show(whole));
        }
        samples.push((whole - overhead.median).max(0.0) / repeats as f64);
    }
    samples.sort_by(f64::total_cmp);
    Ok(Cell {
        took: Number {
            median: percentile(&samples, 50),
            iqr: percentile(&samples, 75) - percentile(&samples, 25),
        },
        repeats,
    })
}

/// How many copies of the query it takes to fill a script.
///
/// Doubling from one, the same way [`crate::timing`] calibrates its inner loop, and for the same
/// reason: dividing an estimate would mean trusting one timed run, which is the measurement this is
/// trying not to make. The runs it throws away are also the warmup, so by the time a sample is
/// taken the file is in the page cache on both sides.
fn calibrate(
    engine: &Path,
    scratch: &Path,
    threads: Option<usize>,
    sql: &str,
    overhead: Number,
    name: &str,
) -> Result<usize, String> {
    let mut repeats = 1;
    loop {
        let script = repeated(scratch, name, sql, repeats)?;
        let whole = went(engine, &script, threads)?.as_nanos() as f64;
        if (whole - overhead.median).max(0.0) >= TARGET_NANOS || repeats >= MAX_REPEATS {
            return Ok(repeats);
        }
        repeats = (repeats * 2).min(MAX_REPEATS);
    }
}

/// Write a script holding `repeats` copies of one statement.
fn repeated(scratch: &Path, name: &str, sql: &str, repeats: usize) -> Result<PathBuf, String> {
    let mut text = String::with_capacity((sql.len() + 2) * repeats);
    for _ in 0..repeats {
        text.push_str(sql);
        // A create table twice over is an error the second time, so every statement that makes one
        // drops it again. Both engines pay for the drop and it is the same drop, so it lands in
        // both cells rather than in the difference between them.
        if sql.contains("CREATE TABLE") {
            text.push_str(";\nDROP TABLE loaded");
        }
        text.push_str(";\n");
    }
    write(scratch, &format!("{name}-script.sql"), &text)
}

/// Put a script somewhere both engines can read it.
fn write(scratch: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    let path = scratch.join(name);
    std::fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// Run one script and say how long the whole process took.
///
/// Output goes nowhere. Every shape here answers with one row, so formatting it is not where the
/// time is, but a script holding twenty thousand of them would spend real time writing to a pipe
/// that nobody reads, and that time is the harness's rather than the engine's.
fn went(engine: &Path, script: &Path, threads: Option<usize>) -> Result<Duration, String> {
    let mut command = Command::new(engine);
    command.arg("-batch").arg("-bail").arg("-noheader");
    if let Some(count) = threads {
        command.arg("-cmd").arg(format!("SET threads = {count}"));
    }
    command.arg("-f").arg(script).stdout(Stdio::null()).stderr(Stdio::piped());
    let at = Instant::now();
    let out = command.output().map_err(|e| format!("could not run {}: {e}", engine.display()))?;
    let took = at.elapsed();
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} refused {}: {}", engine.display(), script.display(), said.trim()));
    }
    Ok(took)
}

/// Where duckdb is, which is `RUDB_DUCKDB` or the path.
fn duckdb() -> Result<PathBuf, String> {
    if let Some(set) = std::env::var_os("RUDB_DUCKDB") {
        let path = PathBuf::from(set);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("RUDB_DUCKDB is {}, which is not a file", path.display()));
    }
    let name = binary("duckdb");
    let Some(path) = std::env::var_os("PATH") else {
        return Err("there is no PATH to look for duckdb on".into());
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "no duckdb on PATH, and RUDB_DUCKDB is not set".to_string())
}

/// What has to be read with the table.
fn caveats() -> Vec<String> {
    vec![
        "  rule two: every cell is the median of nine samples and the percentage after it is the"
            .to_string(),
        "    interquartile range over that median. Double figures means the machine was busy and"
            .to_string(),
        "    the run should be taken again.".to_string(),
        "  rule seven: one machine and one build of each engine, so do not put a cell from this"
            .to_string(),
        "    table next to a cell from a run on another one.".to_string(),
        "  rule ten: the end to end number these explain is ClickBench over hits, which is what"
            .to_string(),
        "    tamnd/rudb-bench measures. A shape here is a slice of one of those queries and not a"
            .to_string(),
        "    replacement for running them.".to_string(),
        "  every repeat but the first reads a file the operating system has cached, and duckdb"
            .to_string(),
        "    caches the bytes a second time inside itself, so both sides are warm. Nothing here"
            .to_string(),
        "    says what a first cold touch of the file costs.".to_string(),
        "  the ratio is rudb over duckdb, so above one is rudb losing and the goal is 0.10."
            .to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{MAX_REPEATS, SHAPES, caveats};

    #[test]
    fn every_shape_has_somewhere_to_put_the_file() {
        for shape in SHAPES {
            assert!(shape.sql.contains("{}"), "{} has no file in it", shape.name);
            assert!(!shape.adds.is_empty(), "{} does not say what it adds", shape.name);
        }
    }

    #[test]
    fn the_ladder_starts_with_the_shape_that_reads_no_columns() {
        assert_eq!(SHAPES[0].name, "count");
        assert!(SHAPES[0].sql.contains("count(*)"));
    }

    #[test]
    fn the_repeat_ceiling_leaves_a_script_a_person_could_open() {
        // Twenty thousand copies of the longest statement here is a few megabytes, which is a file
        // rather than a problem. A ceiling ten times higher would not be.
        let longest = SHAPES.iter().map(|shape| shape.sql.len()).max().unwrap_or(0);
        assert!(longest * MAX_REPEATS < 16 * 1024 * 1024);
    }

    #[test]
    fn the_caveats_say_which_way_round_the_ratio_is() {
        let text = caveats().join("\n");
        assert!(text.contains("rudb over duckdb"));
        assert!(text.contains("warm"));
    }
}
