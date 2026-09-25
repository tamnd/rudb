//! How many queries of a suite the compiled engine would take, and why it turns the rest away.
//!
//! This is the number the second exit criterion of C1 asks for: the refusal rate on JOB and on
//! TPC-H, reported and not gated. Nothing is run. Every table is created from a Parquet file of
//! the same name, every query is put through `EXPLAIN (CODEGEN)`, and a query counts as refused
//! when the explain says `refused:`. So the data only has to have the right columns, and the small
//! fixtures `tamnd/rudb-bench` commits are enough.
//!
//! The queries come from one `.sql` file with a `-- qNN` line before each query, which is how
//! `crates/rudb/testdata/tpch.sql` is laid out, or from a directory with one query per file, which
//! is how the JOB queries are kept. In a directory, `schema.sql` and `fkindexes.sql` are skipped.
//!
//! A refusal names the first thing the compiled engine could not take, so a query with a join and
//! a window is counted under the join only. The totals by reason say what to build next, not
//! everything that is missing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_common::Value;

pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let [tables, queries] = args else {
        return Err("usage: cargo xtask refusals <directory of parquet files> <queries>".into());
    };
    let database = Database::new();
    let created = create(&database, Path::new(tables))?;
    let queries = read(Path::new(queries))?;
    println!();
    println!("tables  {created} from {tables}");
    println!("queries {} from {}", queries.len(), args[1]);
    println!();

    let mut accepted = 0;
    let mut failed = Vec::new();
    let mut reasons: BTreeMap<String, (Vec<String>, String)> = BTreeMap::new();
    for (name, sql) in &queries {
        let verdict = match explain(&database, sql) {
            Ok(text) => match text.strip_prefix("refused: ") {
                Some(refusal) => {
                    let what = refusal.split(": ").next().unwrap_or(refusal).to_string();
                    let entry = reasons.entry(what).or_insert_with(|| (Vec::new(), refusal.into()));
                    entry.0.push(name.clone());
                    format!("refused  {refusal}")
                }
                None => {
                    accepted += 1;
                    "accepted".to_string()
                }
            },
            Err(e) => {
                failed.push(name.clone());
                format!("failed   {e}")
            }
        };
        println!("{name:<6} {verdict}");
    }

    let refused = queries.len() - accepted - failed.len();
    let rate = |n: usize| 100.0 * n as f64 / queries.len().max(1) as f64;
    println!();
    println!(
        "accepted {accepted} ({:.0}%), refused {refused} ({:.0}%), failed to plan {}",
        rate(accepted),
        rate(refused),
        failed.len()
    );
    if !reasons.is_empty() {
        println!();
        println!("refusals, by what was refused first:");
        let mut by_count: Vec<_> = reasons.into_iter().collect();
        by_count.sort_by_key(|(_, (names, _))| std::cmp::Reverse(names.len()));
        for (what, (names, example)) in by_count {
            println!("  {:>3}  {what}, for example {example}", names.len());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("{} queries did not plan: {}", failed.len(), failed.join(" ")))
    }
}

/// One table per Parquet file in `dir`, named after the file.
fn create(database: &Database, dir: &Path) -> Result<usize, String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|e| e == "parquet") {
            files.push(path);
        }
    }
    files.sort();
    for path in &files {
        let path = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        let Some(table) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let sql = format!(
            "CREATE TABLE {table} AS SELECT * FROM read_parquet('{}', binary_as_string=True)",
            path.display()
        );
        database.execute(&sql).map_err(|e| format!("creating {table}: {e}"))?;
    }
    if files.is_empty() {
        return Err(format!("no parquet files in {}", dir.display()));
    }
    Ok(files.len())
}

/// The text `EXPLAIN (CODEGEN)` prints for one query.
fn explain(database: &Database, sql: &str) -> Result<String, String> {
    let result = database.query(&format!("EXPLAIN (CODEGEN) {sql}")).map_err(|e| e.to_string())?;
    let rows: Vec<Vec<Value>> = result.rows().collect();
    match rows.as_slice() {
        [row] => match row.get(1) {
            Some(Value::Varchar(text)) => Ok(text.clone()),
            other => Err(format!("the explain printed {other:?}")),
        },
        other => Err(format!("the explain printed {} rows", other.len())),
    }
}

/// The named queries in a file with `-- qNN` headers, or in a directory of one query per file.
fn read(path: &Path) -> Result<Vec<(String, String)>, String> {
    let unreadable = |p: &Path, e: std::io::Error| format!("reading {}: {e}", p.display());
    if !path.is_dir() {
        let text = std::fs::read_to_string(path).map_err(|e| unreadable(path, e))?;
        return Ok(headed(&text));
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| unreadable(path, e))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .filter(|p| !p.file_stem().is_some_and(|s| s == "schema" || s == "fkindexes"))
        .collect();
    files.sort_by_key(|p| numbered(p));
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|e| unreadable(&file, e))?;
        let name = file.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let sql = text.trim().trim_end_matches(';').trim().to_string();
        found.push((name, sql));
    }
    Ok(found)
}

/// `10a` after `9d`, which a plain sort of the names does not give.
fn numbered(path: &Path) -> (u32, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let digits: String = stem.chars().take_while(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(u32::MAX), stem.to_string())
}

/// The queries of a file laid out like `crates/rudb/testdata/tpch.sql`.
fn headed(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut name = String::new();
    let mut sql = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("--") {
            let rest = rest.trim();
            if rest.starts_with('q')
                && rest.len() > 1
                && rest[1..].chars().all(|c| c.is_ascii_digit())
            {
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
            if !name.is_empty() {
                found.push((name.clone(), statement.to_string()));
            }
            sql.clear();
        }
    }
    found
}
