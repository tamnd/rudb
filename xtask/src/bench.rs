//! The in-repo micro-benchmark table.
//!
//! This measures the part of the engine that exists. Today that is the front end: text in, tokens,
//! a parse tree, an AST out. It is deliberately not the same thing as `tamnd/rudb-bench`, which
//! measures whole queries against whole engines and is the only place a number anybody quotes ever
//! comes from. This one answers a smaller question that comes up on a Tuesday, which is whether the
//! change you are about to push made the parser slower.
//!
//! Two rules from `spec/15-rudb-bench.md` are the reason this file is longer than a loop and a
//! `println!`.
//!
//! Rule two says the median of at least five runs with the interquartile range, never a minimum.
//! A minimum is the run where the scheduler happened to leave you alone, and optimizing against it
//! optimizes for a machine nobody has. So every number here is the median of nine samples, each
//! sample being an inner loop calibrated to run for at least ten milliseconds, and the spread is
//! printed next to it rather than left out.
//!
//! Rule ten says a micro-benchmark number never appears without the end-to-end number it is
//! supposed to explain. rudb cannot run a query yet, so there is no query time to put underneath
//! this table, and the honest thing is to say so in the output every time rather than to let a
//! parser number stand on its own and start sounding like a result. The end-to-end column here is
//! the whole front end, and it is measured rather than summed from the three stage columns, so the
//! stages are an explanation of a number rather than its definition.

use std::hint::black_box;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use rudb_parse::generated::rules::PROGRAM;
use rudb_parse::{Token, Tree, parse_ast, parse_tokens, tokenize, transform};

/// How many samples each number is the median of. Rule two says at least five. Nine is used
/// because it makes the two quartiles land on real samples with three samples on either side of
/// the median, which five does not.
const SAMPLES: usize = 9;

/// How long one sample's inner loop has to run before the sample counts.
///
/// `Instant` on the platforms this runs on resolves to somewhere between tens and hundreds of
/// nanoseconds, and the fastest thing in the table is a few hundred nanoseconds. Timing one call
/// would be measuring the clock. Ten milliseconds is four to five orders of magnitude above the
/// resolution, which puts the clock's contribution below the rounding in the last printed digit.
const SAMPLE_NANOS: f64 = 10_000_000.0;

/// A ceiling on the calibration so a pathological case cannot spin forever.
const MAX_ITERS: u32 = 1 << 26;

/// The workload.
///
/// This is not the parser's test corpus and it must not become it. The corpus grows every time
/// somebody covers another piece of the grammar, which is exactly what a corpus is for and exactly
/// what makes it useless as a workload: a total that gets bigger because a test was added looks
/// identical to a total that got bigger because the parser got slower. So this list is its own
/// thing, and it changes only in a commit that changes it on purpose, with the numbers from before
/// and after in the pull request, because the two totals are not comparable across such a commit.
///
/// Every case has to reach an AST. The transformer answers every rule in the table and one of its
/// answers is a Not implemented error, which is a fast path that does none of the work the transform
/// column claims to be timing. A window function or a `CREATE TABLE` in here today would put a small
/// number in that column and read as the transformer being quick. There is a test for it, which is
/// also what will notice when M1 makes one of them eligible.
///
/// The shapes are chosen to have different costs rather than to be representative of anything. A
/// literal is the floor, the ClickBench query is what the project is actually pointed at, and the
/// nested parentheses are there because a chain of unary precedence rules is where a PEG matcher
/// with a bad FIRST filter falls apart.
const WORKLOAD: &[(&str, &str)] = &[
    ("select literal", "SELECT 1"),
    ("arithmetic", "SELECT 1 + 2 * 3 - 4 / 5 % 6 + 7 * 8 - 9"),
    ("filter and project", "SELECT a, b, c FROM t WHERE a = 1 AND b > 2 OR NOT c"),
    ("group by", "SELECT count(*), sum(x), avg(y) FROM t GROUP BY a, b HAVING count(*) > 1"),
    ("three way join", "SELECT * FROM a LEFT JOIN b USING (id) INNER JOIN c ON c.id = a.id"),
    (
        "order by and limit",
        "SELECT a, b FROM t ORDER BY a ASC, b DESC NULLS LAST LIMIT 10 OFFSET 5",
    ),
    ("set operation", "SELECT a FROM t UNION ALL SELECT b FROM u EXCEPT SELECT c FROM v"),
    ("derived table", "SELECT s.x FROM (SELECT y AS x FROM u WHERE u.k = 1) s WHERE s.x > 1"),
    ("case expression", "SELECT CASE WHEN a THEN 1 WHEN b THEN 2 WHEN c THEN 3 ELSE 4 END FROM t"),
    ("casts and literals", "SELECT CAST(a AS INTEGER), b::VARCHAR, 1.5, 'text', TRUE, NULL FROM t"),
    ("nested parentheses", "SELECT (((((((((a + 1))))))))) FROM t"),
    (
        "clickbench q13",
        "SELECT \"SearchPhrase\", count(*) AS c FROM hits WHERE \"SearchPhrase\" <> '' GROUP BY \"SearchPhrase\" ORDER BY c DESC LIMIT 10",
    ),
];

/// One row of the table.
struct Row {
    name: &'static str,
    bytes: usize,
    tokens: usize,
    steps: u64,
    tokenize: Number,
    matching: Number,
    transform: Number,
    total: Number,
}

/// A median with the spread that says whether to believe it.
#[derive(Clone, Copy)]
struct Number {
    /// Nanoseconds per call, the median of [`SAMPLES`] samples.
    median: f64,
    /// The interquartile range, in nanoseconds.
    iqr: f64,
}

impl Number {
    /// The interquartile range as a fraction of the median, which is the form that can be compared
    /// between a row that takes 200 nanoseconds and a row that takes 20 microseconds.
    fn relative(self) -> f64 {
        if self.median == 0.0 { 0.0 } else { self.iqr / self.median }
    }
}

/// Produce the table.
///
/// A debug build of this would be measuring the borrow checker's leftovers and not the parser, and
/// a number from one would be wrong by a factor that changes with every edit. `cargo xtask` is an
/// alias for a debug `cargo run`, so rather than documenting a longer command that people will get
/// wrong, this re-runs itself under the `bench` profile and measures there. The discriminator is
/// `debug_assertions` rather than a flag, so there is no way to ask for the number that does not
/// mean anything.
pub(crate) fn run(root: &Path) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return rebuild(root);
    }
    report();
    Ok(())
}

/// Re-run this task under the `bench` profile.
fn rebuild(root: &Path) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    println!("building xtask under the bench profile, because a debug number is not a number");
    let status = Command::new(cargo)
        .current_dir(root)
        .args(["run", "--quiet", "--profile", "bench", "--package", "xtask", "--", "bench"])
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if status.success() { Ok(()) } else { Err("the bench build did not run".to_string()) }
}

/// Measure every case and print the table.
fn report() {
    let rows: Vec<Row> = WORKLOAD.iter().map(|&(name, sql)| measure(name, sql)).collect();

    println!("rudb front end, text to AST, on a frozen workload of {} statements", rows.len());
    println!("{}", build_line());
    println!();
    println!(
        "{:<20}  {:>5}  {:>6}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>5}",
        "case", "bytes", "tokens", "steps", "tokenize", "match", "transform", "total", "IQR"
    );
    for row in &rows {
        println!(
            "{:<20}  {:>5}  {:>6}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>4.1}%",
            row.name,
            row.bytes,
            row.tokens,
            row.steps,
            show(row.tokenize.median),
            show(row.matching.median),
            show(row.transform.median),
            show(row.total.median),
            row.total.relative() * 100.0
        );
    }

    let bytes: usize = rows.iter().map(|r| r.bytes).sum();
    let tokens: usize = rows.iter().map(|r| r.tokens).sum();
    let steps: u64 = rows.iter().map(|r| r.steps).sum();
    let total: f64 = rows.iter().map(|r| r.total.median).sum();
    println!();
    println!(
        "{:<20}  {:>5}  {:>6}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}",
        "whole workload",
        bytes,
        tokens,
        steps,
        show(rows.iter().map(|r| r.tokenize.median).sum()),
        show(rows.iter().map(|r| r.matching.median).sum()),
        show(rows.iter().map(|r| r.transform.median).sum()),
        show(total)
    );
    let megabytes_per_second = bytes as f64 / total * 1_000.0;
    let per_token = total / tokens as f64;
    println!();
    println!(
        "{megabytes_per_second:.1} MB/s of SQL text over the whole workload, {per_token:.0}ns per token"
    );

    println!();
    for line in &caveats() {
        println!("{line}");
    }
}

/// The things that have to be read with the table and not after it.
///
/// Written as a list that is printed every time rather than as a paragraph in a document, for the
/// same reason `rudb-bench` prints the reasons a result may not be published under every result:
/// a caveat that lives somewhere else is a caveat nobody reads.
fn caveats() -> Vec<String> {
    vec![
        "Read this with the following, and not on its own:".to_string(),
        format!("  rule two: every number is the median of {SAMPLES} samples, each an inner loop"),
        format!(
            "    calibrated to run for at least {}ms. IQR is the interquartile range of the",
            (SAMPLE_NANOS / 1e6) as u64
        ),
        "    total as a percentage of its median, and double figures means the machine was"
            .to_string(),
        "    busy and the run should be taken again.".to_string(),
        "  rule ten: a micro number never appears without the end-to-end number it explains,"
            .to_string(),
        "    and rudb cannot run a query yet. So the total column here is the front end and"
            .to_string(),
        "    not a query time, and a win in it is worth nothing until there is a query time"
            .to_string(),
        "    underneath it.".to_string(),
        "  rule seven: this is one machine, so do not put it next to a number from another."
            .to_string(),
        "  the three stage columns are measured separately and do not have to add up to the"
            .to_string(),
        "    total, which is measured too. The gap is what the measurement itself costs."
            .to_string(),
        "  peak resident memory is not here. A parse builds an arena and frees it, and the"
            .to_string(),
        "    steps and tokens columns describe that better than a high water mark does."
            .to_string(),
        "  whole queries against whole engines are tamnd/rudb-bench, and that is where any"
            .to_string(),
        "    number anybody quotes comes from.".to_string(),
    ]
}

/// What produced these numbers, which rule one says has to travel with them.
fn build_line() -> String {
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map_or_else(
            || "an unknown rustc".to_string(),
            |out| String::from_utf8_lossy(&out.stdout).trim().to_string(),
        );
    format!(
        "{rustc}, bench profile, {} {}, rudb {}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION")
    )
}

/// Measure one case.
///
/// The three stages are timed on inputs that were produced outside the timed region, so the match
/// column is the matcher and not the matcher plus a tokenizer, and the transform column is the
/// transformer and not the whole front end. The total is timed on the text, which is the thing a
/// caller actually has.
fn measure(name: &'static str, sql: &'static str) -> Row {
    let tokens = tokenize(sql).unwrap_or_else(|e| panic!("{name} does not tokenize: {e}"));
    let tree = parse_tokens(sql, &tokens, PROGRAM, true)
        .unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
    transform(sql, &tokens, &tree).unwrap_or_else(|e| panic!("{name} does not transform: {e}"));

    Row {
        name,
        bytes: sql.len(),
        tokens: tokens.len(),
        steps: tree.steps(),
        tokenize: time(|| {
            drop(black_box(tokenize(black_box(sql))));
        }),
        matching: time(|| {
            drop(black_box(matched(sql, &tokens)));
        }),
        transform: time(|| {
            drop(black_box(transform(sql, &tokens, &tree)));
        }),
        total: time(|| {
            drop(black_box(parse_ast(black_box(sql))));
        }),
    }
}

/// The match stage on its own, named so the closure above stays one line.
fn matched(sql: &str, tokens: &[Token]) -> Tree {
    parse_tokens(black_box(sql), black_box(tokens), PROGRAM, true).expect("parses")
}

/// Run one thing enough times to say how long it takes.
fn time(mut once: impl FnMut()) -> Number {
    let iters = calibrate(&mut once);
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iters {
            once();
        }
        samples.push(start.elapsed().as_nanos() as f64 / f64::from(iters));
    }
    samples.sort_by(f64::total_cmp);
    Number {
        median: percentile(&samples, 50),
        iqr: percentile(&samples, 75) - percentile(&samples, 25),
    }
}

/// How many calls it takes to fill one sample.
///
/// Doubling from one rather than dividing an estimate, because the estimate would come from a
/// single timed call, which is the measurement this whole file exists to avoid making. The
/// discarded calibration runs are also the warmup, which is why there is not a separate one.
fn calibrate(once: &mut impl FnMut()) -> u32 {
    let mut iters: u32 = 1;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            once();
        }
        if start.elapsed().as_nanos() as f64 >= SAMPLE_NANOS || iters >= MAX_ITERS {
            return iters;
        }
        iters = iters.saturating_mul(2);
    }
}

/// The nearest rank percentile of a sorted slice.
///
/// Nearest rank rather than interpolated, so every quartile printed is a number that some sample
/// actually produced rather than a point between two that were. With nine samples the quartiles are
/// the third and the seventh, which leaves three real samples outside each of them.
fn percentile(sorted: &[f64], p: usize) -> f64 {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Nanoseconds in the unit a person reads.
fn show(nanos: f64) -> String {
    if nanos >= 1_000_000.0 {
        format!("{:.3}ms", nanos / 1e6)
    } else if nanos >= 1_000.0 {
        format!("{:.3}us", nanos / 1e3)
    } else {
        format!("{nanos:.1}ns")
    }
}

#[cfg(test)]
mod tests {
    use super::{Number, WORKLOAD, caveats, percentile, show, time};

    #[test]
    fn every_case_in_the_workload_reaches_an_ast() {
        // Not a parse check. The transformer answers every rule, and one of its answers is a Not
        // implemented error, which is a fast path that does none of the work the table claims to be
        // timing. A workload case that stops at the parse tree would put a small number in the
        // transform column and look like the transformer was quick.
        let failed: Vec<String> = WORKLOAD
            .iter()
            .filter_map(|&(name, sql)| {
                rudb_parse::parse_ast(sql).err().map(|e| format!("{name}: {e}"))
            })
            .collect();
        assert!(failed.is_empty(), "the benchmark workload does not reach an AST:\n{failed:#?}");
    }

    #[test]
    fn the_workload_is_not_the_test_corpus() {
        // Not a style point. A workload that grows with coverage cannot be compared against itself
        // from last month, because a total that went up because a test was added is indis-
        // tinguishable from a total that went up because the parser got slower.
        assert!(WORKLOAD.len() < 20, "the workload has started collecting cases");
    }

    #[test]
    fn the_case_names_fit_the_column() {
        for &(name, _) in WORKLOAD {
            assert!(name.len() <= 20, "{name} overflows the case column and breaks the table");
        }
    }

    #[test]
    fn quartiles_land_on_samples_that_really_happened() {
        let sorted = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
        assert!((percentile(&sorted, 25) - 3.0).abs() < f64::EPSILON);
        assert!((percentile(&sorted, 50) - 5.0).abs() < f64::EPSILON);
        assert!((percentile(&sorted, 75) - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_single_sample_still_has_a_percentile() {
        assert!((percentile(&[4.0], 25) - 4.0).abs() < f64::EPSILON);
        assert!((percentile(&[4.0], 75) - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_spread_is_relative_to_the_median_and_not_absolute() {
        let slow = Number { median: 20_000.0, iqr: 2_000.0 };
        let fast = Number { median: 200.0, iqr: 20.0 };
        assert!((slow.relative() - fast.relative()).abs() < f64::EPSILON);
    }

    #[test]
    fn a_median_of_zero_does_not_produce_a_spread_of_infinity() {
        assert!((Number { median: 0.0, iqr: 0.0 }.relative()).abs() < f64::EPSILON);
    }

    #[test]
    fn the_units_change_where_the_numbers_do() {
        assert_eq!(show(999.4), "999.4ns");
        assert_eq!(show(1_000.0), "1.000us");
        assert_eq!(show(1_500_000.0), "1.500ms");
    }

    #[test]
    fn a_time_that_is_measured_is_positive_and_has_a_spread() {
        // Runs the real apparatus on the cheapest real thing there is, which is what catches a
        // calibration loop that never terminates or a percentile that indexes off the end.
        let number = time(|| {
            drop(std::hint::black_box(rudb_parse::tokenize(std::hint::black_box("SELECT 1"))));
        });
        assert!(number.median > 0.0);
        assert!(number.iqr >= 0.0);
    }

    #[test]
    fn rule_ten_is_printed_every_time_and_not_remembered() {
        let text = caveats().join("\n");
        assert!(text.contains("rule ten"), "the table can be read as a result without this");
        assert!(text.contains("rule seven"));
        assert!(text.contains("rule two"));
    }
}
