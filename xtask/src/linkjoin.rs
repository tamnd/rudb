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
//! # Why there is a cache argument
//!
//! Section 6.7 is that the rule builds a hash table when the parent side fits in cache, and at scale
//! factor one every TPC-H parent side does. Run with nothing said about the cache, all twenty two
//! queries plan zero link joins and the table is twenty two floor rows and no evidence at all, which
//! is what the first run of this printed. The third argument is `graph_cache_bytes`, the same knob
//! `cargo xtask sections` takes for the same reason, and it moves the crossover rather than the rule:
//! the planner still decides, it decides against a smaller cache. A run that uses it says so at the
//! top, because a table of timings taken with the cache lied about is not a table of a default
//! configuration and should not be read as one.
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
//! A floor query also checks this harness rather than the engine. Its two sides run the same plan, so
//! nothing it measures could have moved, and an interval of its that misses zero is a row this run
//! got wrong. How many of them do that is the rate at which any row gets called wrong, and with
//! eighteen floor queries at ninety five percent about one should, so a single link join query
//! clearing zero means much less than it looks like. That count is printed, with the directions,
//! because floor queries missing zero both ways is an interval behaving normally while all of them
//! missing the same way is the pairing favouring whichever side runs first and would put the same
//! lean under every other row of the table.
//!
//! # The pair is the unit
//!
//! Every repeat keeps both of its timings and the statistics are taken over the differences, rather
//! than the fastest of each side being taken and one delta made from the two minima. The two halves
//! of a pair ran a moment apart on the same machine under whatever else that machine was doing, so
//! most of what a loaded box does to a timing is in both of them and cancels in the difference. Two
//! minima come from two unrelated moments and carry the whole spread of the machine into the answer.
//!
//! This is not a refinement, it is the difference between a run that says something and a run that
//! does not. The first version of this harness took minima, and on a box at load fifteen where each
//! arm spread eight to one it could not resolve a hundred percent. Paired, the same kind of data on
//! the same kind of box resolves ten. So every query gets an interval as well as a delta, and a
//! query whose interval spans zero is reported as unresolved rather than as a win or a loss. A
//! harness that cannot tell should say so, because the alternative is a table of signs read off
//! noise.
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
    /// One pair of milliseconds per repeat: the link join's run and the forced hash join's run.
    pairs: Vec<(f64, f64)>,
    /// What went wrong, where something did.
    complaint: Option<String>,
    /// Whether the two sides answered the same thing.
    agreed: bool,
}

/// What the table's last column says about one query.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The link join was faster by more than this run can explain as noise.
    Won,
    /// The link join was slower by more than this run can explain as noise.
    Lost,
    /// The interval spans zero, so this run does not know which way it went.
    Unresolved,
    /// No link join was planned, so the two sides ran the same plan and this row is the noise.
    Floor,
}

impl Verdict {
    /// What the column prints.
    fn label(&self) -> &'static str {
        match self {
            Self::Won => "won",
            Self::Lost => "lost",
            Self::Unresolved => "inside noise",
            Self::Floor => "no link, floor",
        }
    }
}

impl Outcome {
    /// How many milliseconds the link join took off the forced hash join, one per pair.
    ///
    /// Milliseconds and not a share. A share per pair looks like the tidier thing to average and is
    /// the wrong estimator: its denominator is one timing, so a pair where the control happened to
    /// run fast divides a small difference by a small number and produces a large share, and because
    /// the denominator cannot go below zero while the numerator can go either way the large ones all
    /// land on the same side. Averaging those is biased, not merely noisy. The first run of this
    /// harness averaged shares and its floor queries, which run the same plan on both sides and must
    /// therefore centre on zero, came out between -51.7% and +3.0% with one of them clearing zero
    /// altogether. The differences are what the pairing makes comparable, so the differences are what
    /// gets averaged, and the division happens once at the end.
    fn deltas(&self) -> Vec<f64> {
        self.pairs.iter().map(|(with, without)| without - with).collect()
    }

    /// What the control cost on average, which is what the mean difference is divided by.
    fn scale(&self) -> Option<f64> {
        let count = self.pairs.len();
        let total = self.pairs.iter().map(|(_, without)| without).sum::<f64>();
        (count > 0 && total > 0.0).then(|| total / count as f64)
    }

    /// The share of the forced hash join run that the link join takes off, so positive is a win.
    fn delta(&self) -> Option<f64> {
        let deltas = self.deltas();
        let scale = self.scale()?;
        let count = deltas.len();
        (count > 0).then(|| deltas.iter().sum::<f64>() / count as f64 / scale)
    }

    /// The ninety five percent interval around that share, or nothing from a single pair.
    ///
    /// Taken over the differences in milliseconds and scaled at the end, for the reason
    /// [`Outcome::deltas`] gives. One pair has no spread to estimate a width from. Two or three have
    /// one and it comes out very wide, which is the honest answer rather than a shortcoming: a run of
    /// three repeats on a loaded machine does not know what happened, and an interval that says so is
    /// better than a delta that does not.
    fn interval(&self) -> Option<(f64, f64)> {
        let deltas = self.deltas();
        let scale = self.scale()?;
        if deltas.len() < 2 {
            return None;
        }
        let count = deltas.len() as f64;
        let mean = deltas.iter().sum::<f64>() / count;
        let variance =
            deltas.iter().map(|delta| (delta - mean).powi(2)).sum::<f64>() / (count - 1.0);
        let error = (variance / count).sqrt() * critical(deltas.len() - 1);
        Some(((mean - error) / scale, (mean + error) / scale))
    }

    /// Which of the four things this query is, once the interval has been consulted.
    fn verdict(&self) -> Verdict {
        if self.links == 0 {
            return Verdict::Floor;
        }
        match self.interval() {
            Some((low, _)) if low > 0.0 => Verdict::Won,
            Some((_, high)) if high < 0.0 => Verdict::Lost,
            _ => Verdict::Unresolved,
        }
    }

    /// The middle of the link join's timings, which is the milliseconds column.
    fn link(&self) -> Option<f64> {
        middle(&self.pairs.iter().map(|(with, _)| *with).collect::<Vec<_>>())
    }

    /// The middle of the forced hash join's timings.
    fn hash(&self) -> Option<f64> {
        middle(&self.pairs.iter().map(|(_, without)| *without).collect::<Vec<_>>())
    }
}

/// Loads the directory, runs the suite both ways and prints the per query table.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let Some(data) = args.first() else {
        return Err(
            "cargo xtask linkjoin <directory of tpch parquet files> [repeats] [cache bytes]".into(),
        );
    };
    let repeats = match args.get(1) {
        Some(text) => text.parse::<usize>().map_err(|_| format!("`{text}` is not a count"))?,
        None => 3,
    };
    if repeats == 0 {
        return Err("a run of nothing measures nothing, ask for at least one repeat".into());
    }
    let cache = match args.get(2) {
        Some(text) => Some(text.parse::<u64>().map_err(|_| format!("`{text}` is not a size"))?),
        None => None,
    };
    let data = PathBuf::from(data);
    let queries = queries(root)?;
    let path = scratch();
    let database = load(&data, &path, cache)?;

    println!();
    println!("corpus   {}", data.display());
    println!("repeats  {repeats}, paired, the order of the pair alternating");
    println!("control  SET disabled_optimizers = 'link_join'");
    if repeats < 8 {
        println!(
            "note     {repeats} pairs is few enough that most intervals will span zero. ask for \
             twenty or more where the answer matters"
        );
    }
    println!();
    println!(
        "{:<7}{:>8}{:>10}{:>10}{:>9}{:>20}  verdict",
        "query", "links", "link ms", "hash ms", "delta", "95% interval"
    );
    println!("{}", "-".repeat(84));

    let mut won = Vec::new();
    let mut lost = Vec::new();
    let mut unresolved = Vec::new();
    let mut floor = Vec::new();
    let mut cleared = Vec::new();
    let mut wrong = Vec::new();
    let mut refused = Vec::new();
    for (name, sql) in &queries {
        let outcome = one(&database, sql, repeats);
        if let Some(complaint) = &outcome.complaint {
            refused.push((name.clone(), complaint.clone()));
            println!("{name:<7}{:>8}{:>10}{:>10}{:>9}{:>20}  refused", "-", "-", "-", "-", "-");
            continue;
        }
        let delta = outcome.delta();
        let verdict = outcome.verdict();
        println!(
            "{name:<7}{:>8}{:>10}{:>10}{:>9}{:>20}  {}",
            outcome.links,
            milliseconds(outcome.link()),
            milliseconds(outcome.hash()),
            percent(delta),
            band(outcome.interval()),
            verdict.label()
        );
        if !outcome.agreed {
            wrong.push(name.clone());
            continue;
        }
        let Some(delta) = delta else { continue };
        match verdict {
            Verdict::Won => won.push((name.clone(), delta)),
            Verdict::Lost => lost.push((name.clone(), delta)),
            Verdict::Unresolved => unresolved.push(name.clone()),
            Verdict::Floor => {
                floor.push(delta);
                // The two sides of a floor query ran the same plan, so nothing here could have
                // moved and an interval that misses zero is a row this run called wrong. How often
                // that happens is how often any row of the table is called wrong, which is the only
                // honest thing to read a single link join row against.
                if let Some((low, high)) = outcome.interval() {
                    if low > 0.0 || high < 0.0 {
                        cleared.push((name.clone(), delta));
                    }
                }
            }
        }
    }
    println!("{}", "-".repeat(84));
    println!(
        "delta is the share of the forced hash join run that the link join takes off, so positive \
         means the link join won"
    );
    println!(
        "the verdict is read off the interval and not off the delta, so a query whose interval \
         spans zero is unresolved however large its delta looks"
    );

    if let Some((low, high)) = spread(&floor) {
        println!(
            "the queries that planned no link join ran the same plan twice, and their deltas came \
             out between {} and {}, which is this run's noise",
            percent(Some(low)),
            percent(Some(high))
        );
    }
    println!(
        "\n{} queries planned a link join, {} faster, {} slower and {} this run cannot resolve",
        won.len() + lost.len() + unresolved.len(),
        won.len(),
        lost.len(),
        unresolved.len()
    );
    if !lost.is_empty() {
        println!("\nwhere the link join lost, which section 9 calls a planner rule to fix");
        for (name, delta) in &lost {
            println!("  {name:<5} {}", percent(Some(*delta)));
        }
    }
    if !unresolved.is_empty() {
        println!(
            "\nwhere this run does not know, which is more repeats or a quieter machine and not a \
             result either way: {}",
            unresolved.join(" ")
        );
    }
    if !cleared.is_empty() {
        let up = cleared.iter().filter(|(_, delta)| *delta > 0.0).count();
        let down = cleared.len() - up;
        println!(
            "\n{} of the {} queries that planned no link join had an interval that missed zero \
             anyway, {} of them upward and {} downward:",
            cleared.len(),
            floor.len(),
            up,
            down
        );
        for (name, delta) in &cleared {
            println!("  {name:<5} {}", percent(Some(*delta)));
        }
        // Both directions is what a ninety five percent interval does one row in twenty and says
        // nothing about the harness. One direction is the harness favouring whichever side of the
        // pair runs first or second, which would put the same lean under every other row too.
        if up == 0 || down == 0 {
            println!(
                "they all lean the same way, which is this harness favouring one side of the pair \
                 rather than the engine moving, and it puts that lean under every row above"
            );
        } else {
            println!(
                "they lean both ways, so this is an interval being wrong at the rate a ninety five \
                 percent interval is wrong and not a lean in the harness"
            );
        }
        println!(
            "either way it is the rate at which a row of this table clears zero without anything \
             having moved, so read a single link join row that cleared against it"
        );
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
    // Counted with the rule back on. `disabled_optimizers` is a session setting and the last thing
    // the query before this one did was turn the rule off, so counting first and clearing afterwards
    // reads every plan through the control and reports a corpus that plans no link joins at all.
    // That is exactly what the first run of this said, and it said it about a corpus where
    // `cargo xtask sections` finds four.
    let counted = database
        .execute("SET graph_sections = 'on'")
        .and_then(|_| database.execute("SET disabled_optimizers = ''"))
        .map_err(say);
    let links = match counted {
        Ok(_) => link_joins(database, sql),
        Err(complaint) => {
            return Outcome {
                links: 0,
                pairs: Vec::new(),
                complaint: Some(complaint),
                agreed: false,
            };
        }
    };
    let bare =
        |complaint| Outcome { links, pairs: Vec::new(), complaint: Some(complaint), agreed: false };
    // The warm up, both ways, before anything is timed.
    for forced in [false, true] {
        if let Err(complaint) = once(database, sql, forced) {
            return bare(complaint);
        }
    }
    let mut pairs = Vec::with_capacity(repeats);
    let mut agreed = true;
    for repeat in 0..repeats {
        // The subject first on even repeats and the control first on odd ones.
        let order = if repeat % 2 == 0 { [false, true] } else { [true, false] };
        let mut answers = Vec::new();
        let mut with = 0.0;
        let mut without = 0.0;
        for forced in order {
            match once(database, sql, forced) {
                Ok((rows, taken)) => {
                    if forced {
                        without = taken;
                    } else {
                        with = taken;
                    }
                    answers.push(rows);
                }
                Err(complaint) => return bare(complaint),
            }
        }
        if answers[0] != answers[1] {
            agreed = false;
        }
        pairs.push((with, without));
    }
    Outcome { links, pairs, complaint: None, agreed }
}

/// One run of one query, with the rule taken away or left alone, as its rows and its milliseconds.
fn once(database: &Database, sql: &str, forced: bool) -> Result<(Vec<String>, f64), String> {
    // Both sides read the sections. The control takes one rewrite away and leaves the layer in
    // place, which is the comparison claim C3 names: two plans over one file, not two files.
    database.execute("SET graph_sections = 'on'").map_err(say)?;
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

/// The middle value of some timings, or nothing from none of them.
///
/// The middle rather than the mean, because one repeat that ran while something else on the machine
/// woke up moves a mean of five and does not move their middle. An even count takes the upper of the
/// two in the middle, which is one row of a table being a tenth of a millisecond out and nothing
/// reads the column arithmetically anyway. The comparison is the delta and the delta is paired.
fn middle(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[sorted.len() / 2])
}

/// The two sided ninety five percent Student t multiplier for `df` degrees of freedom.
///
/// A table rather than a computation. The only thing read off the interval is whether it clears
/// zero, and between a step of this table and the exact value there is far less than the timings
/// themselves are doing. The rows thin out above thirty because the value has almost stopped moving
/// by then.
fn critical(df: usize) -> f64 {
    const SMALL: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    match df {
        0 => f64::INFINITY,
        1..=30 => SMALL[df - 1],
        31..=40 => 2.021,
        41..=60 => 2.000,
        61..=120 => 1.980,
        _ => 1.960,
    }
}

/// A delta as the table prints it, or a dash where there was nothing to take one from.
fn percent(delta: Option<f64>) -> String {
    match delta {
        Some(delta) => format!("{:+.1}%", delta * 100.0),
        None => "-".to_string(),
    }
}

/// An interval as the table prints it, or a dash where there were too few pairs for one.
fn band(interval: Option<(f64, f64)>) -> String {
    match interval {
        Some((low, high)) => format!("{} to {}", percent(Some(low)), percent(Some(high))),
        None => "-".to_string(),
    }
}

/// A timing as the table prints it, or a dash where the query never ran.
fn milliseconds(taken: Option<f64>) -> String {
    match taken {
        Some(taken) => format!("{taken:.1}"),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Verdict, band, critical, middle, percent, spread};

    fn timed(pairs: &[(f64, f64)]) -> Outcome {
        Outcome { links: 1, pairs: pairs.to_vec(), complaint: None, agreed: true }
    }

    #[test]
    fn the_delta_is_the_share_of_the_forced_hash_join_the_link_join_takes_off() {
        // Half the time with the link join, so it takes half of the hash join's run off.
        let delta = timed(&[(50.0, 100.0)]).delta().expect("a pair was timed");
        assert!((delta - 0.5).abs() < 1e-9, "{delta}");
        // A link join that cost something is negative, which is the case the table names by query.
        assert!(timed(&[(120.0, 100.0)]).delta().expect("a pair was timed") < 0.0);
    }

    #[test]
    fn the_delta_is_the_mean_of_the_pairs_and_not_of_the_two_middles() {
        // One busy repeat where both sides were slow and one quiet one where both were fast. Each
        // pair says the link join took a fifth off, and so does the mean of them. A harness that
        // took the fastest of each side separately would compare 40 against 200 and say eighty.
        let delta = timed(&[(200.0, 250.0), (40.0, 50.0)]).delta().expect("two pairs were timed");
        assert!((delta - 0.2).abs() < 1e-9, "{delta}");
    }

    #[test]
    fn a_pair_where_the_control_ran_fast_does_not_get_a_vote_the_size_of_the_query() {
        // Three repeats where the two sides tied at ten milliseconds and one where the machine was
        // briefly free and both sides came in around one, the control at half the subject. Averaging
        // a share per pair reads that last pair as minus one hundred percent, weighs it the same as
        // the three that said nothing, and reports the query twenty five percent slower. Almost all
        // of the time this query spends is in pairs that tied, so the answer is close to zero.
        let outcome = timed(&[(10.0, 10.0), (10.0, 10.0), (10.0, 10.0), (1.0, 0.5)]);
        let delta = outcome.delta().expect("four pairs were timed");
        assert!((delta + 0.0164).abs() < 1e-3, "{delta}");
    }

    #[test]
    fn a_query_that_never_ran_has_no_delta_rather_than_a_division() {
        assert_eq!(timed(&[]).delta(), None);
        assert_eq!(timed(&[(0.0, 0.0)]).delta(), None);
    }

    #[test]
    fn one_pair_has_no_interval_and_so_cannot_be_a_win() {
        let outcome = timed(&[(50.0, 100.0)]);
        assert_eq!(outcome.interval(), None);
        // Fifty percent faster and still unresolved, because nothing here says what the spread was.
        assert_eq!(outcome.verdict(), Verdict::Unresolved);
    }

    #[test]
    fn a_verdict_is_read_off_the_interval_and_not_off_the_delta() {
        // Four pairs that all agree the link join is about a fifth faster. The interval clears zero.
        let agreeing = timed(&[(80.0, 100.0), (81.0, 100.0), (79.0, 100.0), (80.0, 100.0)]);
        assert_eq!(agreeing.verdict(), Verdict::Won);
        // The same mean out of pairs that disagree wildly, which is a machine and not an engine.
        let scattered = timed(&[(20.0, 100.0), (140.0, 100.0), (30.0, 100.0), (130.0, 100.0)]);
        let mean = scattered.delta().expect("four pairs were timed");
        assert!((mean - 0.2).abs() < 1e-9, "{mean}");
        assert_eq!(scattered.verdict(), Verdict::Unresolved);
        // And a loss is the same test with the sign turned around.
        let losing = timed(&[(125.0, 100.0), (124.0, 100.0), (126.0, 100.0), (125.0, 100.0)]);
        assert_eq!(losing.verdict(), Verdict::Lost);
    }

    #[test]
    fn a_query_with_no_link_join_is_the_floor_however_fast_it_looked() {
        let mut outcome = timed(&[(80.0, 100.0), (81.0, 100.0), (79.0, 100.0), (80.0, 100.0)]);
        outcome.links = 0;
        assert_eq!(outcome.verdict(), Verdict::Floor);
    }

    #[test]
    fn the_floor_is_the_widest_the_queries_with_no_link_join_moved() {
        assert_eq!(spread(&[]), None);
        let (low, high) = spread(&[0.02, -0.05, 0.01]).expect("three floor deltas");
        assert!((low + 0.05).abs() < 1e-9, "{low}");
        assert!((high - 0.02).abs() < 1e-9, "{high}");
    }

    #[test]
    fn the_middle_of_some_timings_ignores_the_one_repeat_the_machine_ruined() {
        assert_eq!(middle(&[]), None);
        assert_eq!(middle(&[10.0, 11.0, 900.0]), Some(11.0));
    }

    #[test]
    fn the_t_multiplier_falls_as_the_pairs_pile_up_and_then_stops() {
        assert!(critical(1) > critical(5));
        assert!(critical(5) > critical(29));
        assert!(critical(29) > critical(1000));
        assert!((critical(1000) - 1.96).abs() < 1e-9);
    }

    #[test]
    fn a_column_with_nothing_in_it_prints_a_dash_and_not_a_zero() {
        assert_eq!(percent(None), "-");
        assert_eq!(percent(Some(0.0)), "+0.0%");
        assert_eq!(percent(Some(-0.031)), "-3.1%");
        assert_eq!(band(None), "-");
        assert_eq!(band(Some((-0.02, 0.14))), "-2.0% to +14.0%");
    }
}
