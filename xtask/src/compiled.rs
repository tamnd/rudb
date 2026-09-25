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

/// The projection `tamnd/rudb-bench` loads the file through, which turns the integer times and the
/// day number into the types the DDL declares.
const FIXUP: &str = "* REPLACE (make_date(EventDate) AS EventDate, epoch_ms(EventTime * 1000) AS \
                     EventTime, epoch_ms(ClientEventTime * 1000) AS ClientEventTime, \
                     epoch_ms(LocalEventTime * 1000) AS LocalEventTime)";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let Some(first) = args.first() else {
        return Err("usage: cargo xtask compiled <file.parquet> [q1 q2 ...]".into());
    };
    let file = PathBuf::from(first);
    let file =
        file.canonicalize().map_err(|e| format!("could not resolve {}: {e}", file.display()))?;
    let only: Vec<&str> = args[1..].iter().map(String::as_str).collect();

    let (ddl, queries) = statements(root)?;
    let database = Database::new();
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
    println!();
    println!("{:<5} {:>9} {:>9}  verdict", "query", "first", "compiled");

    let mut same = 0;
    let mut ties = 0;
    let mut refused = 0;
    let mut wrong = Vec::new();
    let mut reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
        println!("{name:<5} {:>8.3}s {:>8.3}s  {verdict}", first.seconds, compiled.seconds);
    }

    println!();
    println!("same {same}, same up to ties {ties}, refused {refused}, differ {}", wrong.len());
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

/// What one engine said about one query.
struct Answer {
    rows: Result<Vec<Vec<Value>>, String>,
    seconds: f64,
}

fn answer(database: &Database, engine: &str, sql: &str) -> Answer {
    if let Err(e) = database.execute(&format!("SET engine = '{engine}'")) {
        return Answer { rows: Err(e.to_string()), seconds: 0.0 };
    }
    let began = Instant::now();
    let rows = database.query(sql).map(|r| r.rows().collect()).map_err(|e| e.to_string());
    Answer { rows, seconds: began.elapsed().as_secs_f64() }
}

/// Whether two answers to a query that ends in `LIMIT` differ only in which of a run of tied rows
/// each engine kept.
///
/// The query is run again with the `LIMIT` and `OFFSET` taken off, on both engines. The two full
/// answers have to agree, and every row either engine kept has to be one of them. That proves the
/// rows are right and leaves only the choice among equals, which SQL does not pin down. It does
/// not prove the compiled engine chose from the right run of ties, but its sort is the same
/// comparison the first engine's is, and that is tested on its own in `rudb-qc`.
fn tied(database: &Database, sql: &str, a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
    let Some(at) = sql.rfind(" LIMIT ") else { return false };
    let whole = &sql[..at];
    let (Ok(first), Ok(compiled)) =
        (answer(database, "first", whole).rows, answer(database, "compiled", whole).rows)
    else {
        return false;
    };
    agree(&first, &compiled) && within(a, &first) && within(b, &first)
}

/// Whether every row of `part` is a row of `whole`, counting repeats.
fn within(part: &[Vec<Value>], whole: &[Vec<Value>]) -> bool {
    let mut left: BTreeMap<String, usize> = BTreeMap::new();
    for row in whole {
        *left.entry(format!("{row:?}")).or_default() += 1;
    }
    part.iter().all(|row| match left.get_mut(&format!("{row:?}")) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    })
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
