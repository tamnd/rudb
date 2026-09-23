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
//! # Why an answer that changed is not always a wrong answer
//!
//! Claim S3 is that no rule changes an answer, and on this suite the naive reading of it fails on
//! queries where nothing is wrong. Five of the forty three take ten rows out of a group by where far
//! more than ten rows are tied for the tenth place, and q18 does it with no `ORDER BY` in the
//! statement at all. Which ten of the tied rows come back is whichever ten the aggregate emitted
//! first, so a rule that sizes the table differently returns different rows and has answered the
//! question exactly as well as the run it is being compared against.
//!
//! So a difference is put through the four steps of the tie rule in
//! `spec/bench/tpc-h/04-the-answers.md` section 4.4, which is the same hazard on the same shape over
//! the other suite. [`settle`] is those steps, and step three, the one that runs the query again
//! without its limit, is [`boundary`]. The table has a column for each of the two outcomes the rule
//! allows, because a rule that reorders a result or takes other rows at the cut is worth knowing
//! about even though it is not a failure, and the run only fails on the fourth outcome.
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

use std::hash::{DefaultHasher, Hash, Hasher};
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
    /// The queries that came back with different rows, where the answer without the limit is the same
    /// answer and the difference is which of the rows tied at the cut the limit let through. Step
    /// three of the tie rule, and also not a failure. Named for the same reason as `tied`.
    cut: Vec<String>,
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
            "unstable {}, which answer differently from themselves with nothing changed, so they \
             are compared without their limit rather than row for row",
            arbitrary.join(" ")
        );
    }
    println!();
    if baseline.is_empty() {
        return Err("nothing answered, so there is nothing to ablate".into());
    }

    println!(
        "{:<28}{:>7}{:>7}{:>7}{:>7}{:>7}{:>10}{:>10}",
        "rule", "fired", "quiet", "tied", "cut", "wrong", "median", "floor"
    );
    println!("{}", "-".repeat(83));
    let mut rows = Vec::new();
    for rule in Rule::ALL {
        // The graph sections start off rather than on, so turning them off ablates nothing, and this
        // suite has no join for them to change anyway. `cargo xtask sections` is their ablation.
        if rule == Rule::GraphSections {
            continue;
        }
        let row = ablate(&database, rule, &baseline, repeats);
        println!(
            "{:<28}{:>7}{:>7}{:>7}{:>7}{:>7}{:>10}{:>10}",
            rule.name(),
            row.fired.len(),
            row.quiet,
            row.tied.len(),
            row.cut.len(),
            row.differ.len() + row.broke.len(),
            percent(median(&row.earned)),
            percent(median(&row.floor)),
        );
        rows.push((rule, row));
    }
    println!("{}", "-".repeat(83));
    println!(
        "median is over the queries the rule fired on and floor is over the ones it left alone, \
         both as the share of the run without the rule that the rule takes off"
    );
    println!(
        "tied is the same rows in another order and cut is a different set of the rows tied at the \
         limit, which steps two and three of the tie rule in \
         spec/bench/tpc-h/04-the-answers.md section 4.4 both allow"
    );

    for (rule, row) in &rows {
        if !row.fired.is_empty() {
            println!("\n{} fired on {}", rule.name(), row.fired.join(" "));
        }
        if !row.tied.is_empty() {
            println!("{} reordered {}", rule.name(), row.tied.join(" "));
        }
        if !row.cut.is_empty() {
            println!("{} took other rows at the cut of {}", rule.name(), row.cut.join(" "));
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
/// which of the tie rule's steps this query gets compared by.
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
    /// Whether this query's own answer is the same answer twice, and so whether a difference in it
    /// row for row is attributable to a rule at all.
    ///
    /// ClickBench q19, q33 and q41 order by a count and limit to ten, and on a real file more groups
    /// share the count at the cut than there is room for. Which ten come back is whichever ten the
    /// aggregate emitted first, which is a property of how the threads happened to interleave. Two
    /// runs with identical settings return different rows, so recording that as the rule having
    /// reordered or changed a result would be this tool putting noise in a column somebody reads as
    /// evidence.
    ///
    /// It is not a free pass. A query this is false for skips the first two steps of the tie rule and
    /// is settled by [`boundary`], which is the comparison that is still meaningful for it: the same
    /// query without its limit answers the same thing every time, and if it stops doing that with a
    /// rule off then the rule broke something. What is lost is only the ability to say which of the
    /// two allowed outcomes it was.
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
        match settle(database, &switch, query, &pair.rows) {
            Answer::Same => {}
            Answer::Tied => row.tied.push(query.name.clone()),
            Answer::Cut => row.cut.push(query.name.clone()),
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
    /// The same rows in the same order.
    Same,
    /// The same rows in a different order, which step two of the tie rule says is not a failure.
    Tied,
    /// Different rows, where the answer without the limit is the same answer, so what differs is
    /// which of the rows tied at the cut the limit let through. Step three of the tie rule.
    Cut,
    /// Different rows, with the answer without the limit different too, which is claim S3 failing.
    Differ,
}

/// The two runs of one query, by all four steps of the tie rule.
///
/// Step one is the ordered comparison and it is the answer almost every time. Step two re-sorts both
/// and compares them as multisets, and a match there means the two runs agree on the rows and
/// disagree on an order nothing in the query pinned. A group by whose table was sized differently
/// emits its groups in a different order, so this is the expected outcome of ablating the two sizing
/// rules over a query with no total order, and treating it as a wrong answer would fail the run on
/// the rules working.
///
/// Step three is [`boundary`] and it is why this function needs the database. It is skipped for a
/// query where the first two steps settled the question, per the note in section 4.4 that step three
/// is expensive and belongs where it has already been earned.
///
/// The first two steps are skipped for a query that is not reproducible against itself, per
/// [`Query::compared`], because a reorder or a different set of rows from such a query is a fact
/// about the threads rather than about the rule, and attributing it to the rule in either column
/// would be reading noise. Those go straight to step three, which is the only comparison that means
/// anything for them.
fn settle(database: &Database, switch: &str, query: &Query, rows: &[String]) -> Answer {
    if let Some(settled) = compare(query, rows) {
        return settled;
    }
    match boundary(database, switch, &query.sql) {
        Some(true) => Answer::Cut,
        _ => Answer::Differ,
    }
}

/// The first two steps of the tie rule, or nothing when neither of them settles the question.
///
/// Separate from [`settle`] because these two steps are a comparison of two lists of strings and the
/// third one is two more runs of the query, and the part that is only strings is the part that can be
/// tested without a file to load.
fn compare(query: &Query, rows: &[String]) -> Option<Answer> {
    if rows == query.rows.as_slice() {
        return Some(Answer::Same);
    }
    if query.compared && multiset(rows) == multiset(&query.rows) {
        return Some(Answer::Tied);
    }
    None
}

/// Whether the query answers the same thing both ways once the limit is taken off it.
///
/// Step three of the tie rule, which exists because the first two steps cannot tell a rule that
/// broke an answer from a rule that changed which of the rows tied at the cut got under the limit.
/// ClickBench has both shapes in it. q18 groups by two columns and limits to ten with no `ORDER BY`
/// at all, so which ten rows it returns is whatever the aggregate emitted first and a presized table
/// emits them in a different order. q22, q23, q32 and q33 order by a count and limit to ten, and on
/// a real file the count at the cut is shared by more groups than there is room for. Ablating a
/// sizing rule over any of those changes the answer and changes nothing that was asked for.
///
/// Section 4.4 says to take the key of the last row and count the rows that share it in each
/// unlimited result. What is done instead is stronger on the half that matters and weaker on the
/// other half: the whole unlimited result is compared as a multiset, which subsumes both the
/// deterministic prefix and the total cardinality that step three falls back to. What it does not
/// check is that the rows the limit admitted were the top ones, because that needs the sort key and
/// a tool reading rendered rows does not have it. So a `TopN` that returned ten rows from the middle
/// of a correct answer would be recorded here as a tie at the cut. That gap is the same one
/// rudb-bench issue 148 is open on, and it is worth stating rather than leaving in the difference
/// between what this prints and what section 4.4 asked for.
///
/// Both sides are run here rather than one side being kept from the baseline, because the unlimited
/// result of a group by over a hundred million rows is tens of millions of rows and holding one of
/// those for every query in the suite on the chance it is needed is not worth the memory. It is
/// needed by almost none of them.
fn boundary(database: &Database, switch: &str, sql: &str) -> Option<bool> {
    let whole = unlimited(sql)?;
    turn(database, switch, true)?;
    let on = fingerprint(database, whole)?;
    turn(database, switch, false)?;
    let off = fingerprint(database, whole)?;
    Some(on == off)
}

/// The query without its limit, or nothing if it never had one.
///
/// Cut at the last `LIMIT` in the text, which also takes an `OFFSET` after it, since the unlimited
/// result is the whole answer and not a different window onto it. Text rather than a parse because
/// this suite is forty three known statements with no subquery and no string literal containing the
/// word in any of them, and a rewrite that works on the input it has is better than a rewrite that
/// claims to work on input it will never see.
fn unlimited(sql: &str) -> Option<&str> {
    let upper = sql.to_uppercase();
    let at = upper.rfind(" LIMIT ")?;
    Some(sql[..at].trim_end())
}

/// One query's rows as a count and two accumulators over them, which is a multiset without the rows.
///
/// A sum and an exclusive or of a hash per row, both of which a reordering leaves alone, so two runs
/// that produced the same rows in a different order come out equal here and two runs that produced
/// different rows do not. It is a fingerprint and not a proof: two different multisets can agree on
/// all three numbers. At sixty four bits twice over that is not a thing worth planning around, and
/// what it buys is that the unlimited result of a group by over a hundred million rows is read one
/// row at a time rather than copied into a second list beside the one the engine already made.
fn fingerprint(database: &Database, sql: &str) -> Option<(u64, u64, u64)> {
    let result = database.query(sql).ok()?;
    let mut rows = 0u64;
    let mut sum = 0u64;
    let mut xor = 0u64;
    let mut line = String::new();
    for row in 0..result.len() {
        line.clear();
        for column in 0..result.width() {
            if column > 0 {
                line.push('|');
            }
            line.push_str(&result.text_at(row, column));
        }
        let mut hasher = DefaultHasher::new();
        line.hash(&mut hasher);
        let one = hasher.finish();
        rows += 1;
        sum = sum.wrapping_add(one);
        xor ^= one;
    }
    Some((rows, sum, xor))
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
    use super::{
        Answer, Query, Rule, compare, delta, fingerprint, median, percent, switch, unlimited,
    };

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
        assert_eq!(compare(&query, &got(&["a", "b", "c"])), Some(Answer::Same));
        // Which is what ablating a rule that sized a hash table does to a query with no total order,
        // and failing the run on it would be failing the run on the rule working.
        assert_eq!(compare(&query, &got(&["c", "a", "b"])), Some(Answer::Tied));
    }

    #[test]
    fn a_row_that_is_not_in_the_other_result_goes_on_to_the_run_without_the_limit() {
        // Nothing here is a failure yet. These are the three shapes step three has to look at, and on
        // this suite two of them turn out to be a tie at the cut rather than a broken answer.
        let query = asked(&["a", "b", "c"], true);
        assert_eq!(compare(&query, &got(&["a", "b", "d"])), None);
        assert_eq!(compare(&query, &got(&["a", "b"])), None);
        // A repeated row is a different multiset, which is the count of a group being wrong.
        assert_eq!(compare(&query, &got(&["a", "b", "b"])), None);
    }

    #[test]
    fn a_query_that_does_not_answer_the_same_thing_twice_by_itself_is_never_tied() {
        // q19, q33 and q41 of ClickBench limit ten rows out of thousands that tie at the cut, so the
        // rows differ between two runs with nothing changed at all. A reorder there is a fact about
        // the threads, so recording it against the rule would be putting noise in a column that is
        // read as evidence. It goes to step three instead, which still has something to say about it.
        let query = asked(&["a", "b", "c"], false);
        assert_eq!(compare(&query, &got(&["c", "b", "a"])), None);
        assert_eq!(compare(&query, &got(&["x", "y", "z"])), None);
        // The same rows in the same order is still the same rows in the same order.
        assert_eq!(compare(&query, &got(&["a", "b", "c"])), Some(Answer::Same));
    }

    #[test]
    fn taking_the_limit_off_takes_the_offset_with_it_and_leaves_a_query_that_had_neither_alone() {
        assert_eq!(
            unlimited("SELECT a FROM t GROUP BY a ORDER BY count(*) DESC LIMIT 10"),
            Some("SELECT a FROM t GROUP BY a ORDER BY count(*) DESC")
        );
        // Which is ClickBench q43, where the window under the limit is not the answer either.
        assert_eq!(unlimited("SELECT a FROM t LIMIT 10 OFFSET 1000"), Some("SELECT a FROM t"));
        // Lower case, because the file is not required to shout.
        assert_eq!(unlimited("select a from t limit 10"), Some("select a from t"));
        // A query with no limit has no boundary to be tied at, so a difference in it is a difference.
        assert_eq!(unlimited("SELECT count(*) FROM t"), None);
    }

    #[test]
    fn the_fingerprint_of_a_result_is_the_same_in_any_order_and_not_the_same_for_other_rows() {
        let database = rudb::Database::new();
        let one = fingerprint(&database, "SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c'))");
        let same = fingerprint(&database, "SELECT * FROM (VALUES (3, 'c'), (1, 'a'), (2, 'b'))");
        let other = fingerprint(&database, "SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (4, 'd'))");
        let fewer = fingerprint(&database, "SELECT * FROM (VALUES (1, 'a'), (2, 'b'))");
        assert!(one.is_some());
        assert_eq!(one, same);
        assert_ne!(one, other);
        assert_ne!(one, fewer);
        // A row twice is a different multiset, which is what a count being wrong looks like.
        let twice = fingerprint(&database, "SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (2, 'b'))");
        assert_ne!(one, twice);
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
