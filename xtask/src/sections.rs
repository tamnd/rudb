//! The twenty two TPC-H queries with the graph sections off and then on, compared byte for byte.
//!
//! # What this is
//!
//! `spec/graph/09-measurement.md` section 9.2 is the exit criterion for the whole graph layer and
//! it is one sentence: run the suite with `graph_sections = off` and again with it on, and the two
//! runs must produce identical answers, byte for byte, on every query of every suite. This is that
//! sentence pointed at TPC-H.
//!
//! The reason it is the exit criterion rather than a nice extra is the failure mode. Section 5.1 of
//! `spec/graph/05-execution.md` says the only way this layer produces a wrong answer is a row id
//! used after the operator that invalidated it, and that is a property of a plan shape rather than
//! of a value. A unit test picks a shape and checks it. This picks twenty two shapes nobody wrote
//! for the purpose and checks all of them, and the reference implementation is the engine with a
//! flag flipped rather than a second system, so it costs one extra run of the suite and no
//! maintenance.
//!
//! # Why it is not a committed test
//!
//! The data. TPC-H at any scale worth running is gigabytes and the repository holds none of it, for
//! the same reason `crates/rudb/testdata/clickbench.sql` has no `hits.parquet` beside it at the
//! size that matters. So this is the command that produces the table, pointed at a directory
//! somebody generated, and `crates/rudb/src/tests.rs` holds the committed version of the same
//! comparison over a parent small enough to live in a test.
//!
//! # What it reports and why the second column is the point
//!
//! Twenty two greens over a run where the layer never fired is twenty two greens that say nothing,
//! and that is the easy way to believe this passed. So the table says, per query, how many joins
//! the plan with the sections on reads a link for. A query with none of them is reported as not
//! exercised rather than as a pass, and the summary line counts the two separately.
//!
//! When a query read no link the plan says why, per section 6.7, and those reasons are tallied
//! under the table. A run that reads nothing is the normal first result and the tally is what turns
//! it from a mystery into a fact: nine times out of ten it says the parent fits in cache, which is
//! the rule working rather than the layer failing.
//!
//! # The cache argument
//!
//! Section 6.4's rule declines a link when the projected parent fits in cache, so every TPC-H
//! parent below about scale factor ten fits and nothing reads a link. That is correct and it makes
//! a differential at a small scale factor useless, because the two runs are then the same run.
//!
//! So the second argument is `graph_cache_bytes`, which is what the end to end test in
//! `crates/rudb/src/tests.rs` already uses for the same reason: it asks about the other side of the
//! crossover without generating a parent that really does not fit. Passing a small number turns
//! every eligible join into a link join and makes a scale factor somebody can generate in a minute
//! say something about correctness. It does not make the timings mean anything, and they are not
//! what this tool is for.
//!
//! # What it compares
//!
//! The rendered rows, in the order the engine gave them. Not the plan, which is supposed to differ,
//! and not a sorted copy of the rows, because every TPC-H query but one has an `ORDER BY` and the
//! one that does not is q15's view definition rather than a result. Two runs that disagree on the
//! order of a result somebody asked to be ordered is a difference worth failing on.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rudb::Database;

/// The nine TPC-H relationships, as `SET graph_links` spells them.
///
/// Every foreign key in the schema, which is what makes this a measurement of the layer rather than
/// of the two joins somebody thought would win. A relationship the file cannot build a link for
/// contributes nothing and is not an error, which is `Database::relationships` refusing to act on a
/// declaration rather than this list being wrong.
const LINKS: &str = "lineitem(l_orderkey) -> orders(o_orderkey), \
     lineitem(l_partkey) -> part(p_partkey), \
     lineitem(l_suppkey) -> supplier(s_suppkey), \
     orders(o_custkey) -> customer(c_custkey), \
     partsupp(ps_partkey) -> part(p_partkey), \
     partsupp(ps_suppkey) -> supplier(s_suppkey), \
     customer(c_nationkey) -> nation(n_nationkey), \
     supplier(s_nationkey) -> nation(n_nationkey), \
     nation(n_regionkey) -> region(r_regionkey)";

/// The eight tables, in the order they are loaded, which is parents before children.
///
/// A key map is built over the parent and a link is looked up in it, per section 3.8, so a load
/// that wrote the children first would checkpoint them against a parent that is not there yet.
const TABLES: [&str; 8] =
    ["region", "nation", "supplier", "part", "partsupp", "customer", "orders", "lineitem"];

/// How one query came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The same rows in the same order, with the layer having read at least one link. A pass.
    Same,
    /// The same rows, and no join in the plan read a link, so nothing about the layer was tested.
    Untouched,
    /// The two runs disagree, which is the wrong answer this whole exercise is looking for.
    Differ,
    /// It answered with the sections off and refused with them on. The same failure as `Differ` and
    /// worth its own name, because a query that returns no rows at all is not a wrong answer that
    /// a comparison of rows would ever see.
    Broke,
    /// The query did not run either way, which is the same failure with the layer and without it
    /// and so not this tool's subject. Reported rather than swallowed, because a suite that
    /// silently shrank is a suite whose greens mean less than they look.
    Refused,
}

impl Verdict {
    /// What the table prints.
    const fn label(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Untouched => "same, no link",
            Self::Differ => "DIFFER",
            Self::Broke => "BROKE",
            Self::Refused => "refused",
        }
    }
}

/// How one query came out, with the numbers the table prints and whatever the engine complained.
struct Outcome {
    /// The verdict, which is the column a reader looks at first.
    verdict: Verdict,
    /// The rows the query produced, taken from the run with the sections on.
    rows: usize,
    /// How many joins in the plan read a link.
    links: usize,
    /// Milliseconds with the sections off.
    off_ms: u128,
    /// Milliseconds with the sections on.
    on_ms: u128,
    /// What the engine said, for a query that refused either way round.
    complaint: Option<String>,
}

/// Loads a TPC-H directory, then runs every query both ways and prints the table.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let Some(data) = args.first() else {
        return Err("cargo xtask sections <directory of tpch parquet files> [cache bytes]".into());
    };
    let cache = match args.get(1) {
        Some(text) => Some(text.parse::<u64>().map_err(|_| format!("`{text}` is not a size"))?),
        None => None,
    };
    let data = PathBuf::from(data);
    let queries = queries(root)?;
    let path = scratch();
    let database = load(&data, &path, cache)?;

    println!("{:<6}{:>8}{:>10}{:>16}  ms off / ms on", "query", "links", "rows", "verdict");
    println!("{}", "-".repeat(64));
    let mut same = 0;
    let mut untouched = 0;
    let mut differ = 0;
    let mut broke = 0;
    let mut refused = 0;
    let mut declined: Vec<(&'static str, usize)> = Vec::new();
    let mut complaints: Vec<(String, String)> = Vec::new();
    for (name, sql) in &queries {
        let outcome = one(&database, sql);
        match outcome.verdict {
            Verdict::Same => same += 1,
            Verdict::Untouched => untouched += 1,
            Verdict::Differ => differ += 1,
            Verdict::Broke => broke += 1,
            Verdict::Refused => refused += 1,
        }
        if outcome.links == 0 {
            tally(&mut declined, &plan(&database, sql));
        }
        if let Some(complaint) = outcome.complaint {
            complaints.push((name.clone(), complaint));
        }
        println!(
            "{name:<6}{links:>8}{rows:>10}{:>16}  {off} / {on}",
            outcome.verdict.label(),
            links = outcome.links,
            rows = outcome.rows,
            off = outcome.off_ms,
            on = outcome.on_ms
        );
    }
    println!("{}", "-".repeat(64));
    println!(
        "{same} exercised the layer and agreed, {untouched} agreed with no link read, \
         {differ} differ, {broke} broke, {refused} refused either way"
    );
    if !declined.is_empty() {
        declined.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
        println!("\nwhy the joins that read no link declined, per section 6.7");
        for (reason, count) in &declined {
            println!("  {count:>4}  {reason}");
        }
    }
    if !complaints.is_empty() {
        println!("\nwhat the engine said about the queries that refused");
        for (name, complaint) in &complaints {
            println!("  {name}  {}", complaint.replace('\n', " "));
        }
    }

    drop(database);
    std::fs::remove_file(&path).ok();

    if differ + broke > 0 {
        return Err(format!(
            "{differ} queries answer differently with the sections on and {broke} stopped \
             answering at all"
        ));
    }
    if same == 0 {
        return Err(
            "no query read a link, so this run says nothing about the layer. The tally above says \
             why. If it says the parent fits in cache then the rule is working and the answer is a \
             larger scale factor or a cache argument, which is the second argument to this command"
                .into(),
        );
    }
    Ok(())
}

/// The reasons section 6.7 prints, as a short label and the text to find it by.
///
/// Matched on a fragment rather than the whole sentence because two of them end in numbers that
/// differ per join, and the fragment is chosen to be the part that names the reason.
const REASONS: [(&str, &str); 10] = [
    ("the parent fits in cache", "which fits in cache"),
    ("the parent's projection is too wide", "which is not under"),
    ("no relationship is declared", "no relationship is declared"),
    ("the link is not in the file", "its link is not in the file"),
    ("the join is not one equality over two columns", "not one equality over two columns"),
    ("a right or a full join", "does not answer a right or a full join"),
    ("the parent side is not a stored table", "parent side is not a stored table"),
    ("the child side is not a stored table", "child side is not a stored table"),
    ("the row id no longer names a row of the child", "no longer rows of the child table"),
    ("the row id would reach a count of columns", "counts its input's columns"),
];

/// Adds the reasons one plan gives to the running tally.
fn tally(into: &mut Vec<(&'static str, usize)>, plan: &str) {
    for (label, fragment) in REASONS {
        let found = plan.matches(fragment).count();
        if found == 0 {
            continue;
        }
        match into.iter_mut().find(|(seen, _)| *seen == label) {
            Some((_, count)) => *count += found,
            None => into.push((label, found)),
        }
    }
}

/// One query, both ways.
///
/// The order is off and then on, because off is the answer being trusted and running it first means
/// a crash on the second run leaves the first one's number on the screen.
fn one(database: &Database, sql: &str) -> Outcome {
    let (off, off_ms) = answer(database, sql, false);
    let (on, on_ms) = answer(database, sql, true);
    let links = link_joins(database, sql);
    let bare = |verdict, complaint| Outcome { verdict, rows: 0, links, off_ms, on_ms, complaint };
    let off = match off {
        Ok(off) => off,
        // It refused without the layer, so it would have refused with it, and whatever is wrong
        // with it is wrong somewhere else. The message is carried anyway, because the alternative
        // is a reader wondering whether the tool broke it.
        Err(complaint) => return bare(Verdict::Refused, Some(complaint)),
    };
    let on = match on {
        Ok(on) => on,
        // It answered without the layer and refused with it. That is this tool's subject, and it
        // is the one failure a comparison of rows cannot see, because there are no rows.
        Err(complaint) => return bare(Verdict::Broke, Some(complaint)),
    };
    let rows = on.len();
    let verdict = if off == on {
        if links == 0 { Verdict::Untouched } else { Verdict::Same }
    } else {
        Verdict::Differ
    };
    Outcome { verdict, rows, links, off_ms, on_ms, complaint: None }
}

/// The rendered rows of one query with the sections in the state asked for, and what it took.
///
/// The error is carried rather than discarded. An early version of this returned nothing for a
/// refusal on the grounds that a run which did not happen has no answer to compare, which is true
/// and left the first real finding of this tool reading as the word `refused` and nothing else.
fn answer(database: &Database, sql: &str, on: bool) -> (Result<Vec<String>, String>, u128) {
    let setting = if on { "SET graph_sections = 'on'" } else { "SET graph_sections = 'off'" };
    if let Err(error) = database.execute(setting) {
        return (Err(say(error)), 0);
    }
    let started = Instant::now();
    let result = match database.query(sql) {
        Ok(result) => result,
        Err(error) => return (Err(say(error)), started.elapsed().as_millis()),
    };
    let taken = started.elapsed().as_millis();
    let mut rendered = Vec::with_capacity(result.len());
    for row in 0..result.len() {
        let mut line = String::new();
        for column in 0..result.width() {
            if column > 0 {
                line.push('|');
            }
            line.push_str(&result.text_at(row, column));
        }
        rendered.push(line);
    }
    (Ok(rendered), taken)
}

/// How many joins the plan reads a link for, with the sections on.
///
/// Counted off the plan text, which is what section 6.7 put the word there for. A count and not a
/// yes or no, because a query that reads one link out of five joins and a query that reads all five
/// are different amounts of evidence and the table should say which one this was.
pub(crate) fn link_joins(database: &Database, sql: &str) -> usize {
    plan(database, sql).matches("LinkJoin").count()
}

/// The plan text with the sections on, or nothing if the query does not plan.
pub(crate) fn plan(database: &Database, sql: &str) -> String {
    if database.execute("SET graph_sections = 'on'").is_err() {
        return String::new();
    }
    let Ok(result) = database.query(&format!("EXPLAIN {sql}")) else { return String::new() };
    if result.is_empty() || result.width() < 2 {
        return String::new();
    }
    result.text_at(0, 1)
}

/// The eight tables out of the directory, the relationships declared, and the whole thing written.
pub(crate) fn load(data: &Path, path: &Path, cache: Option<u64>) -> Result<Database, String> {
    let database =
        Database::open(path.to_str().ok_or("the scratch path is not UTF-8")?).map_err(say)?;
    for table in TABLES {
        let file = data.join(format!("{table}.parquet"));
        if !file.exists() {
            return Err(format!("{} is not there", file.display()));
        }
        let from = file.to_str().ok_or("a data path is not UTF-8")?;
        let sql = format!("CREATE TABLE {table} AS SELECT * FROM '{from}'");
        database.execute(&sql).map_err(say)?;
    }
    database.execute(&format!("SET graph_links = '{LINKS}'")).map_err(say)?;
    // The links are built here, against the key maps this same checkpoint writes. Nothing above
    // asked for a link and nothing below builds one, so a query that reads one is reading what this
    // line wrote, which is the whole of what the two runs below differ by.
    database.execute("CHECKPOINT").map_err(say)?;
    // Set after the checkpoint because it has nothing to do with what was written. It is read by
    // the pass, per section 6.4, and the whole of its effect is which side of the crossover a join
    // lands on. Reported rather than applied quietly, because a table of timings taken with the
    // cache lied about is a table nobody should read as a measurement.
    if let Some(bytes) = cache {
        database.execute(&format!("SET graph_cache_bytes = {bytes}")).map_err(say)?;
        println!("graph_cache_bytes is {bytes}, so the rule is being pushed off its crossover\n");
    }
    Ok(database)
}

/// The queries, in file order, as `name` and `sql`.
pub(crate) fn queries(root: &Path) -> Result<Vec<(String, String)>, String> {
    let path = root.join("crates").join("rudb").join("testdata").join("tpch.sql");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut found = Vec::new();
    let mut name = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("--") {
            let rest = rest.trim();
            if rest.starts_with('q') && rest.len() <= 4 {
                name = rest.to_string();
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        found.push((name.clone(), line.trim_end_matches(';').to_string()));
    }
    if found.is_empty() {
        return Err(format!("{} holds no queries", path.display()));
    }
    Ok(found)
}

/// A scratch file beside the other temporary files, named so two runs at once do not collide.
pub(crate) fn scratch() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    std::env::temp_dir().join(format!("rudb-sections-{}-{stamp}.rdb", std::process::id()))
}

/// An engine error as a line.
pub(crate) fn say(error: rudb_common::Error) -> String {
    error.to_string()
}
