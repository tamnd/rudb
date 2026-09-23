//! The per rule table on ClickBench: what each statistics rule fired on, and what it earned there.
//!
//! # What this is
//!
//! `spec/stats/09-measurement.md` section 9.4 says the deliverable of every statistics milestone is a
//! table with one row per rule: the queries it fired on, the median delta on those queries, and the
//! delta on everything else. Section 9.2 says why, and it is the sentence this whole command exists
//! to serve: a directory that ships twenty rules and reports one total cannot say which of them
//! earned anything, and the twenty first gets added on the strength of a number the third one
//! produced.
//!
//! So this runs the forty three ClickBench queries once with everything on and once per rule with
//! that rule off, and reports the pair. It is the statistics half of what `sections` does for the
//! graph layer, and the two are deliberately the same shape.
//!
//! # Why the file has to be loaded rather than read
//!
//! Every rule here consumes a statistic a store wrote, and a table in memory has none. `Table::zones`
//! answers nothing for a memory table, so a run that pointed the queries at the Parquet file through
//! a replacement scan would ablate a layer that was never there and report forty three greens
//! meaning nothing. The file is loaded into a table and checkpointed, which is what writes the
//! statistics sections the rules read, and then the queries run over that.
//!
//! That costs a copy of the data on disk and it is not optional. A hundred million row hits file
//! needs room for the native file beside it.
//!
//! # How a rule is known to have fired
//!
//! The plan text, compared with the rule on and with it off. Identical text means the rule changed
//! nothing about this query and the query is no evidence either way, which is the distinction that
//! makes the table worth reading: a green from a query the rule never touched is the easy way to
//! believe a rule was measured.
//!
//! This is why `rudb_opt::explain` prints the sizing decisions on an aggregate's line. Two of the
//! rules here write a number onto a node and move no operator, so before that they were invisible in
//! the plan and this tool would have reported them as never firing anywhere.
//!
//! # The delta, and the column that says how much of it to believe
//!
//! Per query the two times are taken next to each other, fastest of however many repeats were asked
//! for. Next to each other because a table of deltas where one side was measured ten minutes before
//! the other is a table measuring what else the machine was doing, and fastest rather than mean
//! because the fastest run is the one with the least of that in it. Each side also runs once untimed
//! first and the order of the pair alternates between repeats, both for the reason in [`paired`]:
//! whichever side went second was reading a warmer file and the bias was larger than the effects this
//! table is looking for.
//!
//! The delta is the share of the run without the rule that the rule takes off, so positive means the
//! rule helped. The second delta column is over the queries where the plan did not change, which is
//! to say over queries where the rule provably did no work. Whatever that column reads is the noise
//! floor of the run, and a fired delta smaller than it is not a result. That column is the reason
//! this tool prints two numbers rather than one.
//!
//! # What is not here
//!
//! The space and the build cost of the statistic each rule consumes, which section 9.4 also asks for
//! in the same table. Those come off the file rather than out of a query, and `cargo xtask stats`
//! already reports them per table. Joining the two into one table is worth doing and is not worth
//! guessing at from in here.
//!
//! The graph sections, which are a rule in the same list and have nothing to do on this suite:
//! ClickBench is one table and zero joins. `cargo xtask sections` is that ablation, over TPC-H, where
//! there are joins for it to change.
//!
//! Three of the rules have no implementation behind them yet. Top n seeding, narrowed arithmetic and
//! memory reservation are named in `spec/stats/05-every-query.md` and nothing in the tree reads them,
//! so their rows come out as having fired on nothing. That is the correct reading of the table rather
//! than a failure of it, and it is worth stating out loud because a row of zeroes looks the same
//! either way.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rudb::Database;
use rudb_common::rules::Rule;

/// What one rule's row of the table is printed from.
///
/// The outcomes are kept as lists of query names rather than as counts because the name is the useful
/// half. A count of one query that answers differently is a line somebody has to go and reproduce,
/// and the first thing they need is which query it was.
#[derive(Default)]
struct Row {
    /// The queries whose plan changed and whose answer did not, which is the rule doing its job.
    fired: Vec<String>,
    /// How many queries the rule left the plan of alone, so they say nothing about it either way.
    quiet: usize,
    /// The queries that came back with the same rows in a different order, which step two of the tie
    /// rule says is not a failure. Named anyway, because a rule that reorders a result nobody asked
    /// to be ordered is worth knowing about.
    tied: Vec<String>,
    /// The queries that answered differently, which have to be none.
    differ: Vec<String>,
    /// The queries that stopped answering with the rule off, which also have to be none. The same
    /// failure as `differ` and the one a comparison of rows cannot see, because there are no rows.
    broke: Vec<String>,
    /// The deltas on the queries it fired on.
    earned: Vec<f64>,
    /// The deltas on the queries where the plan did not change, which is the noise floor.
    floor: Vec<f64>,
}

/// Loads the file, then runs the suite once per rule and prints the table.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let Some(first) = args.first() else {
        return Err("usage: cargo xtask ablate <hits.parquet> [repeats]".into());
    };
    let file = PathBuf::from(first);
    if !file.is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    let file =
        file.canonicalize().map_err(|e| format!("could not resolve {}: {e}", file.display()))?;
    let repeats = match args.get(1) {
        Some(text) => text.parse::<usize>().map_err(|_| format!("`{text}` is not a count"))?,
        None => 3,
    };
    if repeats == 0 {
        return Err("a run of nothing measures nothing, ask for at least one repeat".into());
    }

    let queries = queries(root)?;
    let path = scratch();
    let database = load(&file, &path)?;
    let (baseline, refused) = baseline(&database, &queries);

    println!();
    println!("file     {}", file.display());
    println!("queries  {} of {} answer, {} refuse", baseline.len(), queries.len(), refused.len());
    println!("repeats  {repeats}, fastest of each taken");
    let arbitrary: Vec<&str> =
        baseline.iter().filter(|query| !query.compared).map(|query| query.name.as_str()).collect();
    if !arbitrary.is_empty() {
        println!(
            "timed    but not compared, because two runs of it with nothing changed already \
             disagree: {}",
            arbitrary.join(" ")
        );
    }
    println!();
    if baseline.is_empty() {
        return Err("nothing answered, so there is nothing to ablate".into());
    }

    println!(
        "{:<28}{:>7}{:>7}{:>7}{:>7}{:>10}{:>10}",
        "rule", "fired", "quiet", "tied", "wrong", "median", "floor"
    );
    println!("{}", "-".repeat(76));
    let mut rows = Vec::new();
    for rule in Rule::ALL {
        // The graph sections start off rather than on, so turning them off ablates nothing, and this
        // suite has no join for them to change anyway. `cargo xtask sections` is their ablation.
        if rule == Rule::GraphSections {
            continue;
        }
        let row = ablate(&database, rule, &baseline, repeats);
        println!(
            "{:<28}{:>7}{:>7}{:>7}{:>7}{:>10}{:>10}",
            rule.name(),
            row.fired.len(),
            row.quiet,
            row.tied.len(),
            row.differ.len() + row.broke.len(),
            percent(median(&row.earned)),
            percent(median(&row.floor)),
        );
        rows.push((rule, row));
    }
    println!("{}", "-".repeat(76));
    println!(
        "median is over the queries the rule fired on and floor is over the ones it left alone, \
         both as the share of the run without the rule that the rule takes off"
    );
    println!(
        "tied is the same rows in another order, which step two of the tie rule in \
         spec/bench/tpc-h/04-the-answers.md section 4.4 allows"
    );

    for (rule, row) in &rows {
        if !row.fired.is_empty() {
            println!("\n{} fired on {}", rule.name(), row.fired.join(" "));
        }
        if !row.tied.is_empty() {
            println!("{} reordered {}", rule.name(), row.tied.join(" "));
        }
    }
    let nothing: Vec<&str> =
        rows.iter().filter(|(_, row)| row.fired.is_empty()).map(|(rule, _)| rule.name()).collect();
    if !nothing.is_empty() {
        println!("\nfired on nothing in this suite: {}", nothing.join(" "));
    }
    if !refused.is_empty() {
        println!("\nwhat the engine said about the queries that never answered");
        for (name, complaint) in &refused {
            println!("  {name}  {}", complaint.replace('\n', " "));
        }
    }

    drop(database);
    std::fs::remove_file(&path).ok();

    let wrong: Vec<String> = rows
        .iter()
        .filter(|(_, row)| !row.differ.is_empty() || !row.broke.is_empty())
        .map(|(rule, row)| {
            format!("{} on {}", rule.name(), [&row.differ[..], &row.broke[..]].concat().join(" "))
        })
        .collect();
    if !wrong.is_empty() {
        return Err(format!(
            "a rule changed an answer, which is the one failure claim S3 of \
             spec/stats/09-measurement.md does not survive: {}",
            wrong.join(", ")
        ));
    }
    Ok(())
}

/// What every query answers and plans with every rule on, and the complaints of the ones that do not.
///
/// A query that refuses here refuses for a reason that has nothing to do with any rule, since the
/// ablation only ever takes something away. Eight of the forty three compare `EventDate` against a
/// date and the published file holds an unsigned sixteen bit integer there, which is the usual
/// answer and is a property of the file rather than of the engine.
///
/// Every query runs twice, and the second run is the one kept. The first is what says whether the
/// answer is the same answer twice, which is the question [`Query::compared`] holds and which decides
/// whether this query is allowed to speak about a rule at all.
fn baseline(
    database: &Database,
    queries: &[(String, String)],
) -> (Vec<Query>, Vec<(String, String)>) {
    let mut found = Vec::new();
    let mut refused = Vec::new();
    for (name, sql) in queries {
        let first = match answer(database, sql) {
            Ok(rows) => rows,
            Err(complaint) => {
                refused.push((name.clone(), complaint));
                continue;
            }
        };
        let second = match answer(database, sql) {
            Ok(rows) => rows,
            Err(complaint) => {
                refused.push((name.clone(), complaint));
                continue;
            }
        };
        found.push(Query {
            name: name.clone(),
            sql: sql.clone(),
            plan: plan(database, sql),
            compared: multiset(&first) == multiset(&second),
            rows: second,
        });
    }
    (found, refused)
}

/// One query and what it does with every rule on.
struct Query {
    name: String,
    sql: String,
    plan: String,
    rows: Vec<String>,
    /// Whether this query's own answer is the same answer twice, and so whether comparing it against
    /// itself with a rule off says anything.
    ///
    /// ClickBench q19 and q41 both order by a count and limit to ten, and on any real file thousands
    /// of groups share the count at the cut. Which ten come back is whichever ten the aggregate
    /// emitted first, which is a property of how the threads happened to interleave. Two runs with
    /// identical settings return different rows, so a run with a rule off returning different rows
    /// says nothing about the rule, and reporting it as a wrong answer would be this tool crying
    /// wolf on the one claim nobody is allowed to ignore.
    ///
    /// `spec/bench/tpc-h/04-the-answers.md` section 4.4 is the same hazard on the same shape, and its
    /// step three is the check that would let these two queries be compared properly: the rows before
    /// the boundary, plus the cardinality of the result without its limit. That needs the unlimited
    /// result and so it needs the sort key, which is more than a tool reading rendered rows has. So
    /// these queries are timed and not compared, and the header says which ones they were.
    compared: bool,
}

/// One rule off across the whole suite, against the run with it on.
fn ablate(database: &Database, rule: Rule, baseline: &[Query], repeats: usize) -> Row {
    let switch = switch(rule);
    let mut row = Row::default();
    for query in baseline {
        let Some(pair) = paired(database, &query.sql, &switch, repeats) else {
            // It answered twice in the baseline and will not answer now, or the engine refused the
            // setting. Either way it is counted rather than dropped, because a table that quietly
            // lost a query is a table whose greens mean less than they look.
            row.broke.push(query.name.clone());
            continue;
        };
        if turn(database, &switch, false).is_none() {
            row.broke.push(query.name.clone());
            continue;
        }
        let planned = plan(database, &query.sql);
        match compare(query, &pair.rows) {
            Answer::Same => {}
            Answer::Tied => row.tied.push(query.name.clone()),
            Answer::Differ => {
                row.differ.push(query.name.clone());
                continue;
            }
        }
        let delta = delta(pair.on, pair.off);
        if planned == query.plan {
            row.quiet += 1;
            row.floor.push(delta);
        } else {
            row.fired.push(query.name.clone());
            row.earned.push(delta);
        }
    }
    let _ = turn(database, &switch, true);
    row
}

/// The two times for one query, the rule on and the rule off, and the rows the ablated run gave.
struct Paired {
    /// Milliseconds with the rule on, fastest of the repeats.
    on: f64,
    /// Milliseconds with the rule off, fastest of the repeats.
    off: f64,
    /// What the last run with the rule off answered, which is the half the comparison is of.
    rows: Vec<String>,
}

/// One query timed both ways, in one place and with the order of the two swapped between repeats.
///
/// Both halves here rather than in two passes over the suite, so that a pair of numbers is a pair of
/// numbers about the same minute of the same machine.
///
/// The warm up matters more than it looks. The first run of a query over a file that was written a
/// moment ago pays for reading it, and whichever side went first was charged the whole of that and
/// reported the other side as faster. Over the committed ten thousand row fixture that bias read as
/// four percent on every rule, including the rules that changed no plan at all, which is the size of
/// the effects this table exists to find. So each side runs once untimed before anything is measured,
/// and then the order alternates, which takes out whatever the machine did between the two runs of
/// one pair rather than leaving it on one side.
fn paired(database: &Database, sql: &str, switch: &str, repeats: usize) -> Option<Paired> {
    turn(database, switch, true)?;
    once(database, sql)?;
    turn(database, switch, false)?;
    let mut rows = once(database, sql)?.0;
    let mut on = f64::MAX;
    let mut off = f64::MAX;
    for repeat in 0..repeats {
        let order = if repeat % 2 == 0 { [true, false] } else { [false, true] };
        for wanted in order {
            turn(database, switch, wanted)?;
            let (answered, took) = once(database, sql)?;
            if wanted {
                on = on.min(took);
            } else {
                off = off.min(took);
                rows = answered;
            }
        }
    }
    Some(Paired { on, off, rows })
}

/// Puts one rule where it is wanted, or nothing if the engine would not take the statement.
///
/// On is a `RESET` rather than a `SET` to true, so that on means whatever a fresh session means by it
/// and not whatever this tool thinks it means.
fn turn(database: &Database, switch: &str, on: bool) -> Option<()> {
    let sql = if on { format!("RESET {switch}") } else { format!("SET {switch} = false") };
    database.execute(&sql).ok().map(|_| ())
}

/// How the run with a rule off compares against the run with it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// The same rows in the same order, or a query whose order was never reproducible to begin with.
    Same,
    /// The same rows in a different order, which step two of the tie rule says is not a failure.
    Tied,
    /// Different rows.
    Differ,
}

/// The two runs of one query, by the first two steps of the tie rule.
///
/// Step one is the ordered comparison and it is the answer almost every time. Step two re-sorts both
/// and compares them as multisets, and a match there means the two runs agree on the rows and
/// disagree on an order nothing in the query pinned. A group by whose table was sized differently
/// emits its groups in a different order, so this is the expected outcome of ablating the two sizing
/// rules over a query with no total order, and treating it as a wrong answer would fail the run on
/// the rules working.
///
/// A query whose own answer was not reproducible is not compared, per [`Query::compared`].
fn compare(query: &Query, rows: &[String]) -> Answer {
    if rows == query.rows.as_slice() {
        return Answer::Same;
    }
    if !query.compared {
        return Answer::Same;
    }
    if multiset(rows) == multiset(&query.rows) { Answer::Tied } else { Answer::Differ }
}

/// The rows with the order taken out of them, which is what a multiset comparison is over.
fn multiset(rows: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = rows.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted
}

/// The settings spelling of a rule, which is the canonical name with the dot as an underscore.
fn switch(rule: Rule) -> String {
    rule.name().replacen('.', "_", 1)
}

/// The share of `off` that the rule takes off, as a fraction.
///
/// Positive means the run with the rule on was the faster one, which is the rule earning something.
/// Negative is a rule that cost more than it saved on this query, which section 9.4 says happens and
/// says is answered with a threshold rather than a shrug.
fn delta(on: f64, off: f64) -> f64 {
    if off <= 0.0 {
        return 0.0;
    }
    (off - on) / off
}

/// The middle of a list of deltas, or nothing when the list is empty.
///
/// The median and not the mean, because one query that spilled differently is enough to move a mean
/// of a dozen and the question the column answers is what a typical query got.
fn median(of: &[f64]) -> Option<f64> {
    if of.is_empty() {
        return None;
    }
    let mut sorted = of.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = sorted.len() / 2;
    if sorted.len() % 2 == 1 { Some(sorted[at]) } else { Some((sorted[at - 1] + sorted[at]) / 2.0) }
}

/// A delta as the table prints it, or a dash where there was nothing to take a median of.
fn percent(delta: Option<f64>) -> String {
    match delta {
        Some(delta) => format!("{:+.1}%", delta * 100.0),
        None => "-".to_string(),
    }
}

/// One run of one query, with the rows it produced and the milliseconds it took.
///
/// The rows come back as well as the time because they are what the comparison is of, and a run that
/// was timed and a run that was compared had better be the same run of the same statement.
fn once(database: &Database, sql: &str) -> Option<(Vec<String>, f64)> {
    let at = Instant::now();
    let rows = answer(database, sql).ok()?;
    Some((rows, at.elapsed().as_secs_f64() * 1000.0))
}

/// The rendered rows of one query, in the order the engine gave them.
///
/// Rendered rather than compared as values because the point is what a reader would have seen, and
/// not sorted because forty of the forty three have an `ORDER BY` and two runs that disagree about
/// the order of a result somebody asked to be ordered is a difference worth failing on.
fn answer(database: &Database, sql: &str) -> Result<Vec<String>, String> {
    let result = database.query(sql).map_err(say)?;
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
    Ok(rendered)
}

/// The plan text for one query in whatever state the settings are in, or nothing if it will not plan.
fn plan(database: &Database, sql: &str) -> String {
    let Ok(result) = database.query(&format!("EXPLAIN {sql}")) else { return String::new() };
    if result.is_empty() || result.width() < 2 {
        return String::new();
    }
    result.text_at(0, 1)
}

/// The file as a native table called `hits`, with its statistics sections written.
///
/// The checkpoint is the whole point of the copy. Nothing above it asked for a statistic and nothing
/// below it writes one, so every number the rules read below here is a number this line wrote, which
/// is what makes the ablation an ablation of something.
fn load(file: &Path, path: &Path) -> Result<Database, String> {
    let database =
        Database::open(path.to_str().ok_or("the scratch path is not UTF-8")?).map_err(say)?;
    let from = file.to_str().ok_or("the data path is not UTF-8")?;
    println!("loading {from} into {}", path.display());
    let at = Instant::now();
    database
        .execute(&format!("CREATE TABLE hits AS SELECT * FROM '{from}'"))
        .map_err(|error| format!("could not load it: {}", say(error)))?;
    database.execute("CHECKPOINT").map_err(say)?;
    println!("loaded and checkpointed in {:.1}s", at.elapsed().as_secs_f64());
    Ok(database)
}

/// The queries out of the committed ClickBench file, in file order.
///
/// The table is left called `hits`, unlike `differential`, which replaces the name with the file
/// because it points two engines at the Parquet directly. Here the name is a real table.
///
/// The DDL at the top of the file is skipped rather than run, so the columns have the types the file
/// really holds rather than the types the benchmark wishes it held. That difference is the reason
/// eight of the queries refuse, and it is the same difference `differential` reports.
fn queries(root: &Path) -> Result<Vec<(String, String)>, String> {
    let path = root.join("crates").join("rudb").join("testdata").join("clickbench.sql");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;

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
            if !name.is_empty() {
                found.push((name.clone(), statement.to_string()));
            }
            sql.clear();
        }
    }
    if found.len() != 43 {
        return Err(format!("{} holds {} queries rather than 43", path.display(), found.len()));
    }
    Ok(found)
}

/// A scratch file beside the other temporary files, named so two runs at once do not collide.
fn scratch() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    std::env::temp_dir().join(format!("rudb-ablate-{}-{stamp}.rdb", std::process::id()))
}

/// An engine error as a line.
fn say(error: rudb_common::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::{Answer, Query, Rule, compare, delta, median, percent, switch};

    /// A baseline query that answered those rows, and that either was or was not reproducible.
    fn asked(rows: &[&str], compared: bool) -> Query {
        Query {
            name: "q1".to_owned(),
            sql: "SELECT 1".to_owned(),
            plan: String::new(),
            rows: rows.iter().map(|row| (*row).to_string()).collect(),
            compared,
        }
    }

    fn got(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| (*row).to_string()).collect()
    }

    #[test]
    fn the_same_rows_in_the_same_order_agree_and_in_another_order_are_tied() {
        let query = asked(&["a", "b", "c"], true);
        assert_eq!(compare(&query, &got(&["a", "b", "c"])), Answer::Same);
        // Which is what ablating a rule that sized a hash table does to a query with no total order,
        // and failing the run on it would be failing the run on the rule working.
        assert_eq!(compare(&query, &got(&["c", "a", "b"])), Answer::Tied);
    }

    #[test]
    fn a_row_that_is_not_in_the_other_result_is_the_failure_this_is_looking_for() {
        let query = asked(&["a", "b", "c"], true);
        assert_eq!(compare(&query, &got(&["a", "b", "d"])), Answer::Differ);
        assert_eq!(compare(&query, &got(&["a", "b"])), Answer::Differ);
        // A repeated row is a different multiset, which is the count of a group being wrong.
        assert_eq!(compare(&query, &got(&["a", "b", "b"])), Answer::Differ);
    }

    #[test]
    fn a_query_that_does_not_answer_the_same_thing_twice_by_itself_is_not_evidence() {
        // q19 and q41 of ClickBench limit ten rows out of thousands that tie at the cut, so the rows
        // differ between two runs with nothing changed at all. Calling that a wrong answer would be
        // this tool crying wolf on claim S3, which is the one nobody is allowed to ignore.
        let query = asked(&["a", "b", "c"], false);
        assert_eq!(compare(&query, &got(&["x", "y", "z"])), Answer::Same);
    }

    #[test]
    fn every_rule_has_a_settings_spelling_the_engine_would_accept() {
        // The dot becomes one underscore and the underscores inside a rule's own name survive, which
        // is `rudb_common::rules::canonical` read backwards. A spelling this got wrong would be a
        // whole row of the table measuring a statement the engine refused.
        assert_eq!(switch(Rule::Presize), "stats_presize");
        assert_eq!(switch(Rule::DirectAddressing), "stats_direct_addressing");
        assert_eq!(switch(Rule::TopNSeed), "stats_top_n_seed");
        assert_eq!(switch(Rule::StatsAll), "stats_all");
        for rule in Rule::ALL {
            let spelling = switch(rule);
            assert_eq!(Rule::from_name(&spelling), Some(rule), "{spelling} names something else");
        }
    }

    #[test]
    fn the_delta_is_the_share_of_the_slower_run_the_rule_takes_off() {
        // Half the time with the rule on, so the rule takes half of it off.
        assert!((delta(50.0, 100.0) - 0.5).abs() < 1e-9);
        // The same either way round is no delta, and a rule that cost something is negative.
        assert!(delta(100.0, 100.0).abs() < 1e-9);
        assert!(delta(110.0, 100.0) < 0.0);
    }

    #[test]
    fn a_run_that_took_no_time_at_all_is_no_delta_rather_than_a_division() {
        assert!(delta(0.0, 0.0).abs() < 1e-9);
    }

    #[test]
    fn the_median_of_an_even_count_is_between_the_middle_two() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[0.25]), Some(0.25));
        assert_eq!(median(&[0.4, 0.1, 0.3]), Some(0.3));
        assert_eq!(median(&[0.0, 0.1, 0.3, 0.4]), Some(0.2));
    }

    #[test]
    fn a_column_with_nothing_in_it_prints_a_dash_and_not_a_zero() {
        // A zero would read as a rule that fired and earned nothing, which is a finding. Nothing to
        // take a median of is not a finding, and the two have to look different.
        assert_eq!(percent(None), "-");
        assert_eq!(percent(Some(0.0)), "+0.0%");
        assert_eq!(percent(Some(-0.031)), "-3.1%");
    }
}
