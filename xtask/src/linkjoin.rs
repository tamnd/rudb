//! Claim C3: the link join against the hash join, per query, with the hash join forced.
//!
//! # What this is
//!
//! `spec/graph/09-measurement.md` claim C3 is that the link join beats the hash join where the
//! planner says it should, per query, on the join it was chosen for, with the hash join forced as the
//! control. It is killed per query, and the document is explicit that a query where the link join
//! loses is a planner rule to fix rather than a claim to withdraw, unless it loses everywhere. So
//! this prints the losers by name and does not average them away.
//!
//! The corpus is the same twenty two TPC-H queries out of the same directory of Parquet files that
//! `cargo xtask sections` uses, loaded the same way, with the same nine relationships declared and
//! the same checkpoint writing the links. That is deliberate: the differential and this share a
//! loader so that a query behaving differently between the two is about the question being asked and
//! not about how the file was built.
//!
//! # The control
//!
//! `SET disabled_optimizers = 'link_join'` takes the rule away and leaves everything else where it
//! was, so the control is this engine planning this query with one choice removed, in the same
//! process, over the same file, a moment before or after the subject ran. That is the comparison the
//! claim names. Rebuilding the file without links would have measured a different file as well as a
//! different plan.
//!
//! # Which queries are evidence
//!
//! Only the ones that plan a link join. A query the planner never chose the rule for says nothing
//! about whether the rule was a good choice, and counting it as a green is the easy way to publish a
//! table of greens. Those queries are still timed, both ways, because between them they say what the
//! machine was doing while the others were measured. That is the floor, and it is the same idea as
//! the floor column of `cargo xtask ablate`: a per query delta smaller than the noise is not a
//! result, and the only honest way to know the noise is to measure something that cannot have moved.
//!
//! # What is not here
//!
//! Anything about answers being equal beyond this run's own check. The byte for byte differential
//! over the whole suite is `cargo xtask sections` and it is a separate exit criterion. This checks
//! the two sides agree because a faster wrong answer is not a result, and leaves the thorough version
//! where it already lives.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rudb::Database;

use crate::sections::{link_joins, load, queries, say, scratch};

/// What one query's line of the table is printed from.
struct Outcome {
    /// How many joins of the plan read a link. Zero means the query is the floor and not evidence.
    links: usize,
    /// Milliseconds with the link join available, fastest of the repeats.
    with: f64,
    /// Milliseconds with `link_join` disabled, fastest of the repeats.
    without: f64,
    /// What went wrong, where something did.
    complaint: Option<String>,
    /// Whether the two sides answered the same thing.
    agreed: bool,
}

impl Outcome {
    /// The share of the forced hash join run that the link join takes off, so positive is a win.
    fn delta(&self) -> Option<f64> {
        if self.without <= 0.0 {
            return None;
        }
        Some((self.without - self.with) / self.without)
    }
}

/// Loads the directory, runs the suite both ways and prints the per query table.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let Some(data) = args.first() else {
        return Err("cargo xtask linkjoin <directory of tpch parquet files> [repeats]".into());
    };
    let repeats = match args.get(1) {
        Some(text) => text.parse::<usize>().map_err(|_| format!("`{text}` is not a count"))?,
        None => 3,
    };
    if repeats == 0 {
        return Err("a run of nothing measures nothing, ask for at least one repeat".into());
    }
    let data = PathBuf::from(data);
    let queries = queries(root)?;
    let path = scratch();
    let database = load(&data, &path, None)?;

    println!();
    println!("corpus   {}", data.display());
    println!("repeats  {repeats}, fastest of each taken, the order of the pair alternating");
    println!("control  SET disabled_optimizers = 'link_join'");
    println!();
    println!("{:<7}{:>8}{:>12}{:>12}{:>10}", "query", "links", "link ms", "hash ms", "delta");
    println!("{}", "-".repeat(70));

    let mut won = Vec::new();
    let mut lost = Vec::new();
    let mut floor = Vec::new();
    let mut wrong = Vec::new();
    let mut refused = Vec::new();
    for (name, sql) in &queries {
        let outcome = one(&database, sql, repeats);
        if let Some(complaint) = &outcome.complaint {
            refused.push((name.clone(), complaint.clone()));
            println!("{name:<7}{:>8}{:>12}{:>12}{:>10}  refused", "", "", "", "");
            continue;
        }
        let delta = outcome.delta();
        println!(
            "{name:<7}{:>8}{:>12.1}{:>12.1}{:>10}  {}",
            outcome.links,
            outcome.with,
            outcome.without,
            percent(delta),
            if outcome.links == 0 { "no link, floor" } else { "" }
        );
        if !outcome.agreed {
            wrong.push(name.clone());
            continue;
        }
        let Some(delta) = delta else { continue };
        if outcome.links == 0 {
            floor.push(delta);
        } else if delta < 0.0 {
            lost.push((name.clone(), delta));
        } else {
            won.push((name.clone(), delta));
        }
    }
    println!("{}", "-".repeat(70));
    println!(
        "delta is the share of the forced hash join run that the link join takes off, so positive \
         means the link join won"
    );

    let band = spread(&floor);
    if let Some((low, high)) = band {
        println!(
            "the queries that planned no link join ran the same plan twice, and they came out \
             between {} and {}, which is this run's noise",
            percent(Some(low)),
            percent(Some(high))
        );
    }
    println!(
        "\n{} queries planned a link join, {} of them faster and {} of them slower",
        won.len() + lost.len(),
        won.len(),
        lost.len()
    );
    if !lost.is_empty() {
        println!("\nwhere the link join lost, which section 9 calls a planner rule to fix");
        for (name, delta) in &lost {
            println!("  {name:<5} {}", percent(Some(*delta)));
        }
    }
    if !wrong.is_empty() {
        println!("\nanswered differently with the rule and without it: {}", wrong.join(" "));
    }
    if !refused.is_empty() {
        println!("\nwhat the engine said about the queries that never answered");
        for (name, complaint) in &refused {
            println!("  {name}  {}", complaint.replace('\n', " "));
        }
    }

    drop(database);
    std::fs::remove_file(&path).ok();

    if !wrong.is_empty() {
        return Err(format!(
            "the link join changed an answer on {}, which stops the claim rather than costing it a \
             query",
            wrong.join(" ")
        ));
    }
    Ok(())
}

/// One query timed both ways, with the order of the two swapped between repeats.
///
/// Both sides in one place and alternating for the reason `ablate` gives at more length: whichever
/// side runs second over a file that was written a moment ago is reading a warmer file, and that bias
/// was larger than the effects this table is looking for. Each side also runs once untimed first.
fn one(database: &Database, sql: &str, repeats: usize) -> Outcome {
    let links = link_joins(database, sql);
    let bare = |complaint| Outcome {
        links,
        with: 0.0,
        without: 0.0,
        complaint: Some(complaint),
        agreed: false,
    };
    // The warm up, both ways, before anything is timed.
    for forced in [false, true] {
        if let Err(complaint) = once(database, sql, forced) {
            return bare(complaint);
        }
    }
    let mut with = f64::INFINITY;
    let mut without = f64::INFINITY;
    let mut agreed = true;
    for repeat in 0..repeats {
        // The subject first on even repeats and the control first on odd ones.
        let order = if repeat % 2 == 0 { [false, true] } else { [true, false] };
        let mut answers = Vec::new();
        for forced in order {
            match once(database, sql, forced) {
                Ok((rows, taken)) => {
                    if forced {
                        without = without.min(taken);
                    } else {
                        with = with.min(taken);
                    }
                    answers.push(rows);
                }
                Err(complaint) => return bare(complaint),
            }
        }
        if answers[0] != answers[1] {
            agreed = false;
        }
    }
    Outcome { links, with, without, complaint: None, agreed }
}

/// One run of one query, with the rule taken away or left alone, as its rows and its milliseconds.
fn once(database: &Database, sql: &str, forced: bool) -> Result<(Vec<String>, f64), String> {
    let setting = if forced {
        "SET disabled_optimizers = 'link_join'"
    } else {
        "SET disabled_optimizers = ''"
    };
    database.execute(setting).map_err(say)?;
    let started = Instant::now();
    let result = database.query(sql).map_err(say)?;
    let taken = started.elapsed().as_secs_f64() * 1000.0;
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
    // Sorted, because a query with no `ORDER BY` is allowed to hand its rows back in any order and
    // the question here is whether the two runs hold the same rows, not which order they arrived in.
    rendered.sort_unstable();
    Ok((rendered, taken))
}

/// The lowest and the highest of the floor deltas, or nothing when there were none.
fn spread(floor: &[f64]) -> Option<(f64, f64)> {
    if floor.is_empty() {
        return None;
    }
    let low = floor.iter().copied().fold(f64::INFINITY, f64::min);
    let high = floor.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some((low, high))
}

/// A delta as the table prints it, or a dash where there was nothing to take one from.
fn percent(delta: Option<f64>) -> String {
    match delta {
        Some(delta) => format!("{:+.1}%", delta * 100.0),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, percent, spread};

    fn timed(with: f64, without: f64) -> Outcome {
        Outcome { links: 1, with, without, complaint: None, agreed: true }
    }

    #[test]
    fn the_delta_is_the_share_of_the_forced_hash_join_the_link_join_takes_off() {
        // Half the time with the link join, so it takes half of the hash join's run off.
        let delta = timed(50.0, 100.0).delta().expect("both sides were timed");
        assert!((delta - 0.5).abs() < 1e-9, "{delta}");
        // A link join that cost something is negative, which is the case the table names by query.
        assert!(timed(120.0, 100.0).delta().expect("both sides were timed") < 0.0);
    }

    #[test]
    fn a_query_that_never_ran_has_no_delta_rather_than_a_division() {
        assert_eq!(timed(0.0, 0.0).delta(), None);
    }

    #[test]
    fn the_floor_is_the_widest_the_queries_with_no_link_join_moved() {
        assert_eq!(spread(&[]), None);
        let (low, high) = spread(&[0.02, -0.05, 0.01]).expect("three floor deltas");
        assert!((low + 0.05).abs() < 1e-9, "{low}");
        assert!((high - 0.02).abs() < 1e-9, "{high}");
    }

    #[test]
    fn a_column_with_nothing_in_it_prints_a_dash_and_not_a_zero() {
        assert_eq!(percent(None), "-");
        assert_eq!(percent(Some(0.0)), "+0.0%");
        assert_eq!(percent(Some(-0.031)), "-3.1%");
    }
}
