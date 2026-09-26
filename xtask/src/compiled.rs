//! The forty three ClickBench queries through the first engine and through the compiled engine, in
//! one process, over one file.
//!
//! This is the differential check of `spec/compiler/15-correctness.md` pointed at ClickBench, and
//! the command behind the C1 exit criterion that all forty three are correct under
//! `SET engine = 'compiled'`. The file is loaded into `hits` once, the way `tamnd/rudb-bench` loads
//! it for rudb, and then every query runs under `first` and under `compiled` and the rows are
//! compared. A query the compiled engine refuses still answers, on the first engine, so what it
//! counts as is a refusal, and the refusals are printed grouped by reason at the end, which is the
//! list of what to build next.
//!
//! `--tier` picks the tier the compiled engine runs on, which is how C2's exit criterion, the same
//! answers on `clif`, is checked, and `--suite` points the same comparison at TPC-H or JOB: the
//! tables and the queries are read the way `cargo xtask refusals` reads them. `--tiers <seed>` is
//! the tier against tier differential of section 15.3: see [`tiers`].
//!
//! Rows are compared as sorted lists, because a query with no `ORDER BY` has no order to hold
//! either engine to, and a double is compared to a relative 1e-9 because two engines adding the
//! same numbers in a different order do not have to agree on the last bit. A query that orders and
//! then cuts with `LIMIT` can still differ on ties, so a query that differs is run again without
//! its `LIMIT` to tell a tie from a bug, see [`tied`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rudb::Database;
use rudb_common::Value;

use crate::refusals;

/// The projection `tamnd/rudb-bench` loads the file through, which turns the integer times and the
/// day number into the types the DDL declares.
const FIXUP: &str = "* REPLACE (make_date(EventDate) AS EventDate, epoch_ms(EventTime * 1000) AS \
                     EventTime, epoch_ms(ClientEventTime * 1000) AS ClientEventTime, \
                     epoch_ms(LocalEventTime * 1000) AS LocalEventTime)";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let usage = || {
        "usage: cargo xtask compiled [--threads <n>] [--set <name>=<value>]... [--tier auto|interp|clif|direct | --tiers <seed>] \
         <file.parquet> [q1 q2 ...]\n       \
         cargo xtask compiled [--threads <n>] [--tier auto|interp|clif|direct | --tiers <seed>] \
         --suite <parquet dir> <queries> [q1 ...]\n       \
         cargo xtask compiled [--threads <n>] [--tiers <seed>] --corpus <slt dir>"
            .to_string()
    };
    let mut args = args;
    let mut tier = "auto".to_string();
    let mut seed = None;
    let mut threads = None;
    let mut sets = Vec::new();
    while let [flag, value, rest @ ..] = args {
        match flag.as_str() {
            "--tier" => tier = value.clone(),
            "--tiers" => {
                seed = Some(value.parse::<u64>().map_err(|e| format!("--tiers {value}: {e}"))?);
            }
            "--threads" => {
                threads =
                    Some(value.parse::<u32>().map_err(|e| format!("--threads {value}: {e}"))?);
            }
            "--set" => {
                let (name, set) = value
                    .split_once('=')
                    .ok_or_else(|| format!("--set {value}: not name=value"))?;
                sets.push(format!("SET {name} = '{set}'"));
            }
            _ => break,
        }
        args = rest;
    }
    // The differential compares `interp` with one native tier, `clif` unless `--tier` names
    // another.
    if seed.is_some() && (tier == "auto" || tier == "interp") {
        tier = if cfg!(feature = "qc-clif") { "clif" } else { "direct" }.to_string();
    }
    if let [flag, dir] = args
        && flag == "--corpus"
    {
        return corpus(Path::new(dir), &tier, seed.unwrap_or(1), threads);
    }
    let database = Database::new();
    if let Some(threads) = threads {
        database.execute(&format!("SET threads = {threads}")).map_err(|e| e.to_string())?;
    }
    for set in &sets {
        database.execute(set).map_err(|e| format!("{set}: {e}"))?;
        println!("{set}");
    }
    // The tier is set before anything runs, so a build without it fails here and not after a
    // load of ten million rows.
    database.execute(&format!("SET qc_tier = '{tier}'")).map_err(|e| e.to_string())?;
    let (queries, only) = match args {
        [flag, tables, queries, only @ ..] if flag == "--suite" => {
            let began = Instant::now();
            let created = refusals::create(&database, Path::new(tables))?;
            println!();
            println!(
                "tables  {created} from {tables}, loaded in {:.1}s",
                began.elapsed().as_secs_f64()
            );
            (refusals::read(Path::new(queries))?, only)
        }
        [first, only @ ..] if !first.starts_with("--") => {
            let file = PathBuf::from(first);
            let file = file
                .canonicalize()
                .map_err(|e| format!("could not resolve {}: {e}", file.display()))?;
            let (ddl, queries) = statements(root)?;
            database.execute(&ddl).map_err(|e| format!("the hits DDL: {e}"))?;
            let load = format!(
                "INSERT INTO hits SELECT {FIXUP} FROM read_parquet('{}', binary_as_string=True)",
                file.display()
            );
            let began = Instant::now();
            database.execute(&load).map_err(|e| format!("loading {}: {e}", file.display()))?;
            let rows = database.table_len("hits").map_err(|e| e.to_string())?;
            println!();
            println!("file    {}", file.display());
            println!("rows    {rows}, loaded in {:.1}s", began.elapsed().as_secs_f64());
            (queries, only)
        }
        _ => return Err(usage()),
    };
    let only: Vec<&str> = only.iter().map(String::as_str).collect();
    if let Some(seed) = seed {
        return tiers(&database, &queries, &only, &tier, seed);
    }
    println!("tier    {tier}");
    println!();
    println!("{:<5} {:>9} {:>9} {:>10}  verdict", "query", "first", "compiled", "compile");

    let mut same = 0;
    let mut ties = 0;
    let mut refused = 0;
    let mut wrong = Vec::new();
    let mut reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut compiles = Vec::new();
    for (name, sql) in &queries {
        if !only.is_empty() && !only.contains(&name.as_str()) {
            continue;
        }
        let first = answer(&database, "first", sql);
        let logged = database.refusals().len();
        let compiled = answer(&database, "compiled", sql);
        let refusal = database.refusals().get(logged).cloned();
        let verdict = match (&first.rows, &compiled.rows, &refusal) {
            (_, _, Some(line)) => {
                refused += 1;
                let why = line.split(" | ").next().unwrap_or(line).to_string();
                reasons.entry(why).or_default().push(name.clone());
                "refused".to_string()
            }
            (Ok(a), Ok(b), None) if agree(a, b) => {
                same += 1;
                "same".to_string()
            }
            (Ok(a), Ok(b), None) if tied(&database, sql, a, b) => {
                ties += 1;
                "same up to ties".to_string()
            }
            (Ok(a), Ok(b), None) => {
                wrong.push((name.clone(), shown(a), shown(b)));
                "differ".to_string()
            }
            (Err(a), Err(b), None) if a == b => {
                same += 1;
                "same error".to_string()
            }
            (a, b, None) => {
                let text = |r: &Result<Vec<Vec<Value>>, String>| match r {
                    Ok(rows) => shown(rows),
                    Err(e) => format!("error: {e}"),
                };
                wrong.push((name.clone(), text(a), text(b)));
                "differ".to_string()
            }
        };
        if refusal.is_none() {
            compiles.push(compiled.compile_ms);
        }
        println!(
            "{name:<5} {:>8.3}s {:>8.3}s {:>8.3}ms  {verdict}",
            first.seconds, compiled.seconds, compiled.compile_ms
        );
    }

    println!();
    println!("same {same}, same up to ties {ties}, refused {refused}, differ {}", wrong.len());
    compiled_in(&mut compiles);
    if !reasons.is_empty() {
        println!();
        println!("refusals, by reason:");
        let mut by_count: Vec<_> = reasons.into_iter().collect();
        by_count.sort_by_key(|(_, names)| std::cmp::Reverse(names.len()));
        for (why, names) in by_count {
            println!("  {:>2}  {why}  ({})", names.len(), names.join(" "));
        }
    }
    for (name, first, compiled) in &wrong {
        println!();
        println!("{name} first:    {first}");
        println!("{name} compiled: {compiled}");
    }
    if wrong.is_empty() { Ok(()) } else { Err(format!("{} queries differ", wrong.len())) }
}

/// The tier differential of `spec/compiler/15-correctness.md` section 15.3: every query on
/// `interp`, on a native tier, and on that tier with switches back and forth at random morsels,
/// one thread, and the three answers the same to the bit and in the same order.
///
/// The switches are drawn from `seed` plus the query's place in the list, so a failure names the
/// setting that brings it back. Rows are compared through their `Debug` text, which writes a double
/// with every bit that tells it apart, `-0.0` included. How many switches landed in an aggregate or
/// a join build, where state lives from one morsel to the next, is printed per query, and a run
/// where none landed anywhere fails, because then it tested nothing.
fn tiers(
    database: &Database,
    queries: &[(String, String)],
    only: &[&str],
    native: &str,
    seed: u64,
) -> Result<(), String> {
    let set = |sql: &str| database.execute(sql).map(|_| ()).map_err(|e| format!("{sql}: {e}"));
    set("SET threads = 1")?;
    set("SET engine = 'compiled'")?;
    println!("tiers   interp, {native}, {native} with qc_switch random:{seed}+n, threads 1");
    println!();
    println!(
        "{:<5} {:>9} {:>9} {:>9} {:>9} {:>9}  verdict",
        "query", "interp", native, "switched", "in aggr", "in build"
    );
    let (mut same, mut refused, mut wrong) = (0, 0, Vec::new());
    let start = database.tier_switches();
    for (n, (name, sql)) in queries.iter().enumerate() {
        if !only.is_empty() && !only.contains(&name.as_str()) {
            continue;
        }
        let logged = database.refusals().len();
        let mut runs = Vec::new();
        let before = database.tier_switches();
        for (tier, switch) in [
            ("interp", "off".to_string()),
            (native, "off".to_string()),
            (native, format!("random:{}", seed.wrapping_add(n as u64))),
        ] {
            set(&format!("SET qc_tier = '{tier}'"))?;
            set(&format!("SET qc_switch = '{switch}'"))?;
            let began = Instant::now();
            let rows: Result<Vec<Vec<Value>>, String> =
                database.query(sql).map(|r| r.rows().collect()).map_err(|e| e.to_string());
            runs.push((format!("{rows:?}"), began.elapsed().as_secs_f64(), switch));
        }
        set("SET qc_switch = 'off'")?;
        let after = database.tier_switches();
        let (aggregate, build) = (after.aggregate - before.aggregate, after.build - before.build);
        let switched = after.total() - before.total();
        let verdict = if database.refusals().len() > logged {
            refused += 1;
            "refused".to_string()
        } else if runs.iter().all(|r| r.0 == runs[0].0) {
            same += 1;
            "same".to_string()
        } else {
            let odd: Vec<String> = runs[1..]
                .iter()
                .filter(|r| r.0 != runs[0].0)
                .map(|r| format!("{native}, qc_switch {}", r.2))
                .collect();
            wrong.push((name.clone(), runs[0].0.clone(), odd.join(" and ")));
            format!("differ on {}", odd.join(" and "))
        };
        println!(
            "{name:<5} {:>8.3}s {:>8.3}s {:>8.3}s {switched:>9} {aggregate:>9} {build:>9}  {verdict}",
            runs[0].1, runs[1].1, runs[2].1
        );
    }
    println!();
    println!("same {same}, refused {refused}, differ {}", wrong.len());
    let end = database.tier_switches();
    let (result, aggregate, build) =
        (end.result - start.result, end.aggregate - start.aggregate, end.build - start.build);
    println!(
        "switches {} in all: {} producing rows, {} in an aggregate, {} in a join build",
        result + aggregate + build,
        result,
        aggregate,
        build
    );
    for (name, interp, which) in &wrong {
        println!();
        println!("{name} interp: {}", interp.chars().take(400).collect::<String>());
        println!("{name} differs on {which}");
    }
    if !wrong.is_empty() {
        return Err(format!("{} queries differ between the tiers", wrong.len()));
    }
    if same > 0 && result + aggregate + build == 0 {
        return Err("no switch landed in any query, so the differential tested nothing".into());
    }
    Ok(())
}

/// What one engine said about one query.
struct Answer {
    rows: Result<Vec<Vec<Value>>, String>,
    seconds: f64,
    /// The `codegen_ns` of the query, in milliseconds: zero on the first engine.
    compile_ms: f64,
}

fn answer(database: &Database, engine: &str, sql: &str) -> Answer {
    if let Err(e) = database.execute(&format!("SET engine = '{engine}'")) {
        return Answer { rows: Err(e.to_string()), seconds: 0.0, compile_ms: 0.0 };
    }
    let began = Instant::now();
    let (rows, compile_ms) = match database.query(sql) {
        Ok(result) => (Ok(result.rows().collect()), compile_ms(&result)),
        Err(e) => (Err(e.to_string()), 0.0),
    };
    Answer { rows, seconds: began.elapsed().as_secs_f64(), compile_ms }
}

/// The time a query spent generating and compiling code, from its timing document, in
/// milliseconds.
fn compile_ms(result: &rudb::QueryResult) -> f64 {
    result.metrics().map_or(0.0, |m| m.timing.codegen_ns as f64 / 1e6)
}

/// Every query in a directory of sqllogictest files, on the first engine and on the compiled one
/// on `interp`, on `clif` and on `clif` switching tiers at random morsels.
///
/// This points both differentials at the committed corpus of `tamnd/rudb-compat`, which has the
/// subqueries, windows and edge cases the benchmark suites do not. The files are not checked
/// against their written answers, which is the corpus test's job: every statement runs in order on
/// one database per file, and each query the compiled engine takes has to give the first engine's
/// rows, compared the way the rest of this command compares them, and the same rows bit for bit on
/// every tier. Only the lines a record starts with are read, so a query the harness would skip
/// for a `skipif` still runs here, which only makes it one more query.
fn corpus(dir: &Path, tier: &str, seed: u64, threads: Option<u32>) -> Result<(), String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        // flatten: an entry that cannot be read is a file this run does not see, as `ls` would.
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "test" || e == "slt"))
        .collect();
    files.sort();
    let mut natives = Vec::new();
    if tier != "interp" && cfg!(feature = "qc-clif") {
        natives.extend([("clif", "off".to_string()), ("clif", format!("random:{seed}"))]);
    }
    if tier != "interp" && cfg!(target_arch = "x86_64") {
        natives.extend([("direct", "off".to_string()), ("direct", format!("random:{seed}"))]);
    }
    let (mut queries, mut refused, mut errors, mut same) = (0, 0, 0, 0);
    let mut wrong = Vec::new();
    let mut compiles = Vec::new();
    // Producing rows, in an aggregate, in a join build.
    let mut landed = [0u64; 3];
    for file in &files {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let name = file.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let database = Database::new();
        if let Some(threads) = threads {
            database.execute(&format!("SET threads = {threads}")).map_err(|e| e.to_string())?;
        }
        let before = database.tier_switches();
        for (line, query, sql) in records(&text) {
            if !query {
                let _ = database.execute("SET engine = 'first'");
                // The answer is the corpus test's to check. Here the statement only has to have
                // run, or not, the way it does for the corpus.
                let _ = database.execute(&sql);
                continue;
            }
            queries += 1;
            let first = answer(&database, "first", &sql);
            let logged = database.refusals().len();
            let mut runs = Vec::new();
            for (tier, switch) in
                std::iter::once(("interp", "off".to_string())).chain(natives.clone())
            {
                let set = format!("SET qc_tier = '{tier}'");
                database.execute(&set).map_err(|e| format!("{set}: {e}"))?;
                let set = format!("SET qc_switch = '{switch}'");
                database.execute(&set).map_err(|e| format!("{set}: {e}"))?;
                let ran = answer(&database, "compiled", &sql);
                runs.push((format!("{tier} {switch}"), ran));
            }
            let _ = database.execute("SET qc_switch = 'off'");
            if database.refusals().len() > logged {
                refused += 1;
                continue;
            }
            if let Some((_, ran)) = runs.get(1) {
                compiles.push(ran.compile_ms);
            }
            let Ok(expected) = &first.rows else {
                // An error on the first engine is an answer too, and the compiled engine has to
                // give the same one.
                let odd: Vec<&str> =
                    runs.iter().filter(|r| r.1.rows != first.rows).map(|r| r.0.as_str()).collect();
                if odd.is_empty() {
                    errors += 1;
                } else {
                    wrong.push(format!(
                        "{name}:{line} {} error differs on {}",
                        sql,
                        odd.join(", ")
                    ));
                }
                continue;
            };
            let reference = format!("{:?}", runs[0].1.rows);
            let mut odd = Vec::new();
            for (label, ran) in &runs {
                let agrees = match &ran.rows {
                    Ok(rows) => agree(expected, rows),
                    Err(_) => false,
                };
                if !agrees {
                    odd.push(format!("{label} against first"));
                } else if format!("{:?}", ran.rows) != reference {
                    odd.push(format!("{label} against interp"));
                }
            }
            if odd.is_empty() {
                same += 1;
            } else {
                wrong.push(format!("{name}:{line} {sql}\n  differs: {}", odd.join(", ")));
            }
        }
        let after = database.tier_switches();
        landed[0] += after.result - before.result;
        landed[1] += after.aggregate - before.aggregate;
        landed[2] += after.build - before.build;
    }
    let tiers: Vec<String> = std::iter::once("interp".to_string())
        .chain(natives.iter().map(|(tier, switch)| {
            if switch == "off" { tier.to_string() } else { format!("{tier} switching") }
        }))
        .collect();
    println!("corpus  {} files, {queries} queries, on {}", files.len(), tiers.join(", "));
    println!("same {same}, same error {errors}, refused {refused}, differ {}", wrong.len());
    println!(
        "switches {} in all: {} producing rows, {} in an aggregate, {} in a join build",
        landed.iter().sum::<u64>(),
        landed[0],
        landed[1],
        landed[2]
    );
    compiled_in(&mut compiles);
    for line in &wrong {
        println!();
        println!("{line}");
    }
    if wrong.is_empty() { Ok(()) } else { Err(format!("{} queries differ", wrong.len())) }
}

/// The statements and queries of a sqllogictest file in order: the line each starts on, whether it
/// is a query, and its SQL.
fn records(text: &str) -> Vec<(usize, bool, String)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut at = 0;
    // row at a time: a record is found by its first line and ends at a blank line or `----`.
    while at < lines.len() {
        let head = lines[at].trim();
        let query = head.starts_with("query");
        if !query && !head.starts_with("statement") {
            at += 1;
            continue;
        }
        let start = at + 1;
        at += 1;
        let mut sql = Vec::new();
        while at < lines.len() && !lines[at].trim().is_empty() && lines[at].trim() != "----" {
            sql.push(lines[at]);
            at += 1;
        }
        if !sql.is_empty() {
            out.push((start, query, sql.join("\n")));
        }
    }
    out
}

/// Prints the median, the slowest and the sum of the compile times of the queries the compiled
/// engine took, in milliseconds.
fn compiled_in(compiles: &mut [f64]) {
    if compiles.is_empty() {
        return;
    }
    compiles.sort_by(f64::total_cmp);
    let median = compiles[compiles.len() / 2];
    let slowest = compiles[compiles.len() - 1];
    let sum: f64 = compiles.iter().sum();
    println!(
        "compile time over {} queries: median {median:.3} ms, slowest {slowest:.3} ms, all {sum:.3} ms",
        compiles.len()
    );
}

/// Whether two answers to a query that ends in `LIMIT` differ only in which of a run of tied rows
/// each engine kept.
///
/// The query is run again on the first engine with the `LIMIT` and `OFFSET` taken off, and every
/// row either engine kept has to be one of its rows, counting repeats. That proves the rows are
/// right and leaves only the choice among equals, which SQL does not pin down. It does not prove
/// the compiled engine chose from the right run of ties, but its sort is the same comparison the
/// first engine's is, and that is tested on its own in `rudb-qc`.
///
/// The full answer can be every group of ten million rows (q33), so it is read one row at a time
/// and only the rows the two limited answers hold are counted, instead of keeping it all.
fn tied(database: &Database, sql: &str, a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
    let Some(at) = sql.rfind(" LIMIT ") else { return false };
    if a.len() != b.len() || database.execute("SET engine = 'first'").is_err() {
        return false;
    }
    let Ok(whole) = database.query(&sql[..at]) else { return false };
    let mut wanted: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for row in a {
        wanted.entry(format!("{row:?}")).or_default().0 += 1;
    }
    for row in b {
        wanted.entry(format!("{row:?}")).or_default().1 += 1;
    }
    for row in whole.rows() {
        if let Some(seen) = wanted.get_mut(&format!("{row:?}")) {
            seen.2 += 1;
        }
    }
    wanted.values().all(|&(a, b, seen)| a <= seen && b <= seen)
}

/// Whether two answers hold the same rows, in any order.
fn agree(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let (a, b) = (sorted(a), sorted(b));
    a.iter().zip(&b).all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(l, r)| close(l, r)))
}

fn sorted(rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let mut rows = rows.to_vec();
    rows.sort_by_cached_key(|r| format!("{r:?}"));
    rows
}

fn close(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Double(x), Value::Double(y)) => {
            x == y || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()) || (x.is_nan() && y.is_nan())
        }
        _ => l == r,
    }
}

/// The first few rows of an answer, for a person to read.
fn shown(rows: &[Vec<Value>]) -> String {
    let head: Vec<String> = rows.iter().take(5).map(|r| format!("{r:?}")).collect();
    format!("{} rows {}", rows.len(), head.join(" "))
}

/// The DDL and the forty three queries, from the file the front end test reads.
fn statements(root: &Path) -> Result<(String, Vec<(String, String)>), String> {
    let path = root.join("crates").join("rudb").join("testdata").join("clickbench.sql");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut ddl = String::new();
    let mut found = Vec::new();
    let mut name = String::new();
    let mut sql = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("--") {
            let rest = rest.trim();
            if rest.starts_with('q') && rest[1..].chars().all(|c| c.is_ascii_digit()) {
                name = rest.to_string();
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if !sql.is_empty() {
            sql.push(' ');
        }
        sql.push_str(line);
        if let Some(statement) = sql.strip_suffix(';') {
            if name.is_empty() {
                ddl = statement.to_string();
            } else {
                found.push((name.clone(), statement.to_string()));
            }
            sql.clear();
        }
    }
    if found.len() != 43 || ddl.is_empty() {
        return Err(format!("{} does not hold the DDL and 43 queries", path.display()));
    }
    Ok((ddl, found))
}
