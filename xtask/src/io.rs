//! The I/O table: what a batch of reads costs through the pool and through the loop it replaces.
//!
//! Sub-milestone 2d puts an I/O thread pool underneath `File::submit`, and the argument for it in
//! `spec/engine/05-scan.md` section 5.3 has two halves. One is a correctness of measurement
//! argument, that a thread blocked on a read should not be a core that is not computing, and that
//! one cannot be settled by this table because there is no execution pool to lose a core from yet.
//! The other is that concurrency and coalescing make a batch of reads cheaper than the same reads
//! one at a time, and that one is measurable today.
//!
//! Three access patterns, because the answer is different for each and a single number would be a
//! number about whichever one got picked.
//!
//! A sequential scan in one megabyte ranges, which is what reading a whole column of a row group
//! looks like. A column pattern, sixty four kilobyte ranges with seven ranges skipped between each
//! pair, which is what a projection of one column out of eight looks like and is the pattern
//! coalescing is aimed at. And a page pattern, eight kilobyte ranges at scattered offsets, which
//! is what the page index makes possible and what has the highest per request cost.
//!
//! # The number that matters is not on a warm cache
//!
//! `--cold` drops the page cache between samples, which needs Linux and root. Without it every
//! read after the first is a memcpy out of the page cache, and a table of those says how fast this
//! machine copies memory. Section 5.3 is explicit that a design validated only on a warm laptop
//! NVMe gets this wrong, so the cold column is the one to read and the warm one is there because
//! it is the case a developer runs into.
//!
//! A cold row is one sample rather than nine, because the second sample is not cold. Rule two from
//! `spec/15-rudb-bench.md` asks for the median of nine and a spread, and a cold number cannot have
//! one, so it is printed with the spread column empty and labelled rather than quietly broken.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use rudb_io::{Config, File, Filesystem, OpenMode, Pool, RealFilesystem, Request};

use crate::timing::{Number, SAMPLES, build_line, percentile, rebuild, shared_caveats, show};

/// How big the file under the table is, by default.
///
/// Two hundred and fifty six megabytes is small enough to write in a few seconds and large enough
/// that a cold read of it is a read of a disk rather than of a readahead window.
const DEFAULT_BYTES: u64 = 256 << 20;

/// One access pattern.
struct Pattern {
    name: &'static str,
    /// The ranges it reads, as offset and length.
    ranges: Vec<(u64, usize)>,
}

impl Pattern {
    fn wanted(&self) -> u64 {
        self.ranges.iter().map(|&(_, len)| len as u64).sum()
    }

    fn requests(&self) -> Vec<Request> {
        self.ranges.iter().map(|&(offset, len)| Request::new(offset, len)).collect()
    }
}

fn patterns(bytes: u64) -> Vec<Pattern> {
    let sequential = (0..bytes / (1 << 20)).map(|i| (i << 20, 1 << 20)).collect();
    let column = (0..bytes / (64 << 10))
        .filter(|i| i % 8 == 0)
        .map(|i| (i * (64 << 10), 64 << 10))
        .collect();
    // A deterministic scatter, so two runs read the same offsets and the table is comparable with
    // itself. SplitMix64, the same one `rudb-io` uses to shuffle completions.
    let mut state = 0x5eed_5eed_5eed_5eedu64;
    let slots = bytes / (8 << 10);
    let pages = (0..4096)
        .map(|_| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) % slots * (8 << 10), 8 << 10)
        })
        .collect();
    vec![
        Pattern { name: "sequential 1MiB", ranges: sequential },
        Pattern { name: "column 64KiB", ranges: column },
        Pattern { name: "pages 8KiB", ranges: pages },
    ]
}

/// One way of running a pattern.
#[derive(Clone, Copy)]
enum Engine {
    /// The loop over `read_at` on the calling thread, which is what the pool replaces.
    Loop,
    /// The pool, at this many threads, coalescing over gaps of this many bytes or not at all.
    Pool { threads: usize, gap: Option<u64> },
}

impl Engine {
    fn name(self) -> String {
        match self {
            Self::Loop => "read_at loop".to_string(),
            Self::Pool { threads, gap: None } => format!("pool {threads}"),
            Self::Pool { threads, gap: Some(gap) } => {
                format!("pool {threads} merge {}KiB", gap >> 10)
            }
        }
    }
}

/// One measured cell.
struct Row {
    pattern: &'static str,
    engine: String,
    wanted: u64,
    time: Number,
    reads: u64,
    read: u64,
}

impl Row {
    /// Megabytes a second of the bytes the caller asked for, which is the rate a scan sees.
    fn rate(&self) -> f64 {
        if self.time.median <= 0.0 {
            return 0.0;
        }
        (self.wanted as f64 / (1 << 20) as f64) / (self.time.median / 1e9)
    }

    /// Bytes read off the device over bytes wanted. One means nothing was read that nobody asked
    /// for, and anything above it is coalescing deciding a gap was cheaper to read than to skip.
    fn amplification(&self) -> f64 {
        if self.wanted == 0 { 1.0 } else { self.read as f64 / self.wanted as f64 }
    }
}

/// Runs the table.
///
/// # Errors
///
/// If the scratch file cannot be written, or if `--cold` is asked for on a machine that cannot
/// drop its page cache, because a cold column that is quietly warm is worse than no column.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return rebuild(root, "io", args);
    }
    let cold = args.iter().any(|a| a == "--cold");
    let bytes = args
        .iter()
        .position(|a| a == "--bytes")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BYTES);

    if cold {
        drop_caches().map_err(|e| format!("--cold was asked for and {e}"))?;
    }

    let scratch = Scratch::new(bytes)?;
    let fs = RealFilesystem::new();
    let opened = fs
        .open(scratch.path(), OpenMode::Read)
        .map_err(|e| format!("could not open the scratch file: {e}"))?;
    let file: Arc<dyn File> = Arc::from(opened);

    let engines = vec![
        Engine::Loop,
        Engine::Pool { threads: 1, gap: None },
        Engine::Pool { threads: 2, gap: None },
        Engine::Pool { threads: 4, gap: None },
        Engine::Pool { threads: 8, gap: None },
        Engine::Pool { threads: 16, gap: None },
        // The gap sweep, at the thread count the unmerged rows above are there to pick. Three
        // sizes rather than one because the gap is what decides which patterns merge at all, and a
        // gap large enough to help the scattered pages is large enough to merge the column pattern
        // into a read of everything between the columns.
        Engine::Pool { threads: 16, gap: Some(16 << 10) },
        Engine::Pool { threads: 16, gap: Some(64 << 10) },
        Engine::Pool { threads: 16, gap: Some(512 << 10) },
    ];

    let mut rows = Vec::new();
    for pattern in patterns(bytes) {
        for &engine in &engines {
            rows.push(measure(&file, &pattern, engine, cold));
        }
    }

    report(&rows, bytes, cold);
    Ok(())
}

fn measure(file: &Arc<dyn File>, pattern: &Pattern, engine: Engine, cold: bool) -> Row {
    let wanted = pattern.wanted();

    // The pool is built once and outside every timed region, because a pool is built once in a
    // process and not once per batch. Building it inside would be timing eight thread spawns and
    // eight joins per sample, which is a real cost of something but it is not the cost of a read.
    let pool = match engine {
        Engine::Loop => None,
        Engine::Pool { threads, gap } => {
            let config = match gap {
                Some(gap) => Config::local_disk().with_threads(threads).coalescing(gap),
                None => Config::local_disk().with_threads(threads).not_coalescing(),
            };
            Some(Pool::new(config))
        }
    };

    let once = || match &pool {
        None => {
            for &(offset, len) in &pattern.ranges {
                let mut buf = vec![0u8; len];
                file.read_at(offset, &mut buf).expect("read failed");
            }
        }
        Some(pool) => {
            let completion = pool.submit(file, pattern.requests());
            completion.wait().expect("submit failed");
        }
    };

    // The counters come off a warmup run rather than off the timed ones, as a difference, because
    // the pool's counters accumulate across every batch it has ever served and the row wants the
    // per batch number. The loop has no counters, so its two numbers are what it was asked for.
    let before = pool.as_ref().map(Pool::stats);
    once();
    let (reads, read) = match (&pool, before) {
        (Some(pool), Some(before)) => {
            let after = pool.stats();
            (after.reads - before.reads, after.read - before.read)
        }
        _ => (pattern.ranges.len() as u64, wanted),
    };

    let time = if cold {
        // One sample, because the second one is not cold. Dropping the cache between two samples of
        // one number would be dropping it between the two halves of that number. The warmup above
        // is what filled the cache, so the drop goes after it and immediately before the clock.
        let _ = drop_caches();
        let start = Instant::now();
        once();
        Number { median: start.elapsed().as_nanos() as f64, iqr: f64::NAN }
    } else {
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let start = Instant::now();
            once();
            samples.push(start.elapsed().as_nanos() as f64);
        }
        samples.sort_by(f64::total_cmp);
        Number {
            median: percentile(&samples, 50),
            iqr: percentile(&samples, 75) - percentile(&samples, 25),
        }
    };

    Row { pattern: pattern.name, engine: engine.name(), wanted, time, reads, read }
}

fn report(rows: &[Row], bytes: u64, cold: bool) {
    println!();
    println!(
        "what a batch of reads costs, {} on a {} MiB file",
        if cold { "cold" } else { "warm" },
        bytes >> 20
    );
    println!("  {}", build_line());
    println!();
    println!(
        "{:<18} {:<20} {:>10} {:>10} {:>7} {:>8} {:>7}",
        "pattern", "engine", "time", "MiB/s", "IQR", "reads", "amp"
    );
    let mut last = "";
    for row in rows {
        if row.pattern != last {
            if !last.is_empty() {
                println!();
            }
            last = row.pattern;
        }
        let iqr = if row.time.iqr.is_nan() {
            "one".to_string()
        } else {
            format!("{:.1}%", row.time.relative() * 100.0)
        };
        println!(
            "{:<18} {:<20} {:>10} {:>10.1} {:>7} {:>8} {:>7.2}",
            row.pattern,
            row.engine,
            show(row.time.median),
            row.rate(),
            iqr,
            row.reads,
            row.amplification()
        );
    }
    println!();
    println!("caveats");
    for line in shared_caveats() {
        println!("{line}");
    }
    if cold {
        println!("  a cold row is one sample and not nine, because the second sample is not cold,");
        println!("    so its spread column says `one` rather than a percentage nobody can use.");
    } else {
        println!("  this is warm. Every read after the first is a memcpy out of the page cache,");
        println!("    so this table says how fast this machine copies memory as much as it says");
        println!(
            "    anything about a disk. Run it again with --cold, which needs Linux and root."
        );
    }
    println!("  amp is bytes read over bytes wanted. One means nothing was read that nobody asked");
    println!("    for. Above one is coalescing reading a gap because the gap was cheaper to read");
    println!("    than to skip, which is a decision and not a bug, and it is why the byte count");
    println!("    is two numbers rather than one, per spec/engine/13-measurement.md section 13.5.");
    println!("  rule ten: this is a micro number. The end to end number it is meant to explain is");
    println!("    a ClickBench query over hits.parquet, and it does not exist until the reader");
    println!("    that sits on top of this does.");
}

/// Asks the kernel to forget what it has cached.
fn drop_caches() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("dropping the page cache needs Linux".to_string());
    }
    // `sync` first, because dirty pages are not droppable and the scratch file was just written.
    let _ = std::process::Command::new("sync").status();
    std::fs::write("/proc/sys/vm/drop_caches", "3")
        .map_err(|e| format!("could not drop the page cache, which needs root: {e}"))
}

/// The file the table reads, which removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(bytes: u64) -> Result<Self, String> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("rudb-io-table-{unique}"));
        let fs = RealFilesystem::new();
        let file = fs
            .open(&path, OpenMode::CreateNew)
            .map_err(|e| format!("could not make the scratch file: {e}"))?;
        // A ramp rather than zeroes, so a filesystem that would have made this sparse cannot, and
        // so a read that returns the wrong offset's bytes is visible rather than plausible.
        let block: Vec<u8> = (0..(1 << 20)).map(|i: u32| (i >> 3) as u8).collect();
        let mut at = 0u64;
        while at < bytes {
            let len = block.len().min((bytes - at) as usize);
            file.write_at(at, &block[..len]).map_err(|e| format!("could not write: {e}"))?;
            at += len as u64;
        }
        file.sync().map_err(|e| format!("could not sync: {e}"))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
