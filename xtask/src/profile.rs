//! One TPC-H query, run many times in one configuration, for a profiler to sit on top of.
//!
//! # What this is and what it is not
//!
//! `cargo xtask linkjoin` answers whether the link join won, and it does that by running both plans
//! alternately so the machine's load falls out of the difference. That pairing is what makes it
//! trustworthy on a shared box and it is exactly what a profiler cannot use, because a profile of a
//! run that alternated between two plans is a profile of both of them added together. So this runs
//! one query, one plan, as many times as asked, and nothing else.
//!
//! The milliseconds it prints are therefore not a measurement of anything. They are here so a run
//! can be seen to have done the work before its profile is read, and so an obviously disturbed run
//! can be thrown away rather than explained. Anybody wanting to know whether one plan is faster than
//! another should use `linkjoin` and read its interval.
//!
//! # How it is meant to be used
//!
//! Twice, once per arm, under whatever profiler is to hand:
//!
//! ```text
//! perf record -g -- ./target/release/xtask profile /root/rudb-data/tpch1 q12 20 link
//! perf record -g -- ./target/release/xtask profile /root/rudb-data/tpch1 q12 20 hash
//! ```
//!
//! Both runs load the same eight tables the same way and check the same links in, so that part of
//! each profile is the same work and the difference between the two profiles is the plan. Asking for
//! enough repeats that the query dominates the load is the reader's job: at scale factor one the
//! load is a few seconds and a query is a few hundred milliseconds, so twenty is about where the
//! query stops being a rounding error in its own profile.
//!
//! # Why it names the arm rather than taking a flag
//!
//! `link` and `hash` are what the two plans are called everywhere else in this layer, and a profile
//! sitting in a directory next to another one is easier to tell apart by the word in the command
//! that produced it than by whether a flag was present. The word is also what the header line
//! prints, so a profile pasted into an issue without its command still says which arm it is.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rudb::Database;

use crate::sections::{link_joins, load, queries, say, scratch};

/// Which plan to run, which is which way round `disabled_optimizers` is set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// The plan the planner chose, with every rule available to it.
    Link,
    /// The same query with the link join rule taken away, which is the control `linkjoin` uses.
    Hash,
}

impl Arm {
    /// The word on the command line and in the header.
    const fn label(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Hash => "hash",
        }
    }

    /// What `disabled_optimizers` has to say for this arm.
    const fn setting(self) -> &'static str {
        match self {
            Self::Link => "SET disabled_optimizers = ''",
            Self::Hash => "SET disabled_optimizers = 'link_join'",
        }
    }
}

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let (Some(data), Some(wanted)) = (args.first(), args.get(1)) else {
        return Err(
            "cargo xtask profile <directory of tpch parquet files> <query> [repeats] [link|hash] \
             [cache bytes]"
                .into(),
        );
    };
    let repeats = match args.get(2) {
        Some(text) => text.parse::<usize>().map_err(|_| format!("`{text}` is not a count"))?,
        None => 20,
    };
    if repeats == 0 {
        return Err("a run of nothing profiles nothing, ask for at least one repeat".into());
    }
    let arm = match args.get(3).map(String::as_str) {
        Some("link") | None => Arm::Link,
        Some("hash") => Arm::Hash,
        Some(other) => return Err(format!("`{other}` is neither link nor hash")),
    };
    let cache = match args.get(4) {
        Some(text) => Some(text.parse::<u64>().map_err(|_| format!("`{text}` is not a size"))?),
        None => None,
    };

    let data = PathBuf::from(data);
    let queries = queries(root)?;
    let Some((name, sql)) = queries.iter().find(|(name, _)| name == wanted) else {
        let names = queries.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(" ");
        return Err(format!("there is no {wanted} in the suite, which holds {names}"));
    };

    let path = scratch();
    let database = load(&data, &path, cache)?;
    let outcome = profile(&database, name, sql, repeats, arm);
    drop(database);
    std::fs::remove_file(&path).ok();
    outcome
}

/// Runs the one query the one way, printing what it took each time.
fn profile(
    database: &Database,
    name: &str,
    sql: &str,
    repeats: usize,
    arm: Arm,
) -> Result<(), String> {
    database.execute("SET graph_sections = 'on'").map_err(say)?;
    database.execute(arm.setting()).map_err(say)?;
    // Counted after the setting is applied, so it is the plan this run will actually execute rather
    // than whatever the last statement left the session on. The hash arm is expected to read zero
    // and it is printed anyway, because a hash arm reading one would mean the control did not take.
    let links = link_joins(database, sql);

    println!();
    println!("corpus   {name}, the {} plan", arm.label());
    println!("setting  {}", arm.setting());
    println!("links    {links} joins in this plan read a link");
    println!("repeats  {repeats}, unpaired, so these milliseconds are not a comparison");
    println!();

    // One run before anything is reported, so the first timing is not the one that faulted the file
    // in. It is inside the profile either way, which is fine: the profiler is being pointed at the
    // steady state and one cold run out of twenty one does not hide it.
    once(database, sql)?;
    let mut taken = Vec::with_capacity(repeats);
    for repeat in 0..repeats {
        let milliseconds = once(database, sql)?;
        println!("  {:>3}   {milliseconds:>8.1} ms", repeat + 1);
        taken.push(milliseconds);
    }

    // The median rather than the mean, because a single run that landed on somebody else's
    // compilation moves a mean of twenty by more than the thing being profiled is worth.
    taken.sort_by(f64::total_cmp);
    if let Some(middle) = taken.get(taken.len() / 2) {
        println!();
        println!("median   {middle:.1} ms over {repeats}, lowest {:.1}", taken[0]);
    }
    Ok(())
}

/// One run of the query as the session is currently set, as its milliseconds.
///
/// The rows are counted and dropped. They are not compared against anything, because the arm that
/// would be compared against is not in this process: that is `linkjoin`'s job and it does it on
/// every pair it times.
fn once(database: &Database, sql: &str) -> Result<f64, String> {
    let started = Instant::now();
    let result = database.query(sql).map_err(say)?;
    let taken = started.elapsed().as_secs_f64() * 1000.0;
    drop(result);
    Ok(taken)
}
