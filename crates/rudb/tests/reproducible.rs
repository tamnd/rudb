//! The plan is a function of the data, the generation and the settings, and never of the cache.
//!
//! `spec/stats/04-in-memory.md` section 4.3 states the rule and section 4.2 states the half of it
//! that is about opening: a table is opened by reading the header and the directory, and not by
//! reading statistics. The two are one idea. If opening loads whatever is cheap and planning uses a
//! statistic when it happens to be resident, then the same query over the same data produces two
//! different plans depending on what ran before it, the committed plan baselines start flapping,
//! and a benchmark's second run is faster than its first for a reason that has nothing to do with
//! the engine being good.
//!
//! There are no statistics in the file yet, so today this holds by there being nothing to load. It
//! is worth pinning now for the same reason the ablation in `rules.rs` is: the change that breaks
//! it is the reasonable looking one, made by somebody who noticed that a column summary is a few
//! hundred bytes and the next query is going to want it. What that person will not notice is that
//! the plan moved.

use rudb::Database;
use rudb_catalog::QualifiedName;
use rudb_catalog::table::Rows;
use rudb_native::Reads;

/// Queries with enough shape in them that a cardinality estimate could move a plan if anything let
/// it. A join has a build side to choose, a group by and a sort are both above a filter, and the
/// limit is the one node whose estimate is a ceiling rather than a guess.
const QUERIES: &[&str] = &[
    "SELECT a FROM t WHERE a > 100",
    "SELECT count(*) FROM t",
    "SELECT b, count(*) FROM t WHERE a > 50 GROUP BY b",
    "SELECT a FROM t ORDER BY a DESC LIMIT 10",
    "SELECT l.a, r.b FROM t AS l JOIN t AS r ON l.a = r.a WHERE l.a < 500",
];

/// A file with a table in it, written by one database so that another actually reads it back.
fn written(tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-reproducible-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    let writing = Database::open(name).expect("a file name starts a native database");
    writing
        .execute(
            "CREATE TABLE t AS SELECT r::INTEGER AS a, (r % 97)::BIGINT AS b \
             FROM range(20000) AS s(r)",
        )
        .expect("the table is created");
    writing.execute("CHECKPOINT").expect("the table is committed");
    path
}

/// Every plan in `QUERIES`, as text.
fn plans(database: &Database) -> Vec<String> {
    QUERIES
        .iter()
        .map(|sql| database.plan(sql).unwrap_or_else(|error| panic!("{sql} did not plan: {error}")))
        .collect()
}

/// What the file behind the one table in this database has been read for.
fn reads(database: &Database) -> Reads {
    database.with_catalog(|catalog| {
        let name = QualifiedName::new("memory".to_owned(), "main".to_owned(), "t".to_owned());
        let table = catalog.table(&name).expect("the one table");
        match table.rows() {
            Rows::Native(reader) => reader.reads(),
            Rows::Memory(_) | Rows::Grown(_, _) => {
                panic!("the table has rows in memory, so this test is not testing a file")
            }
        }
    })
}

#[test]
fn opening_a_database_and_planning_a_query_reads_no_rows() {
    // Section 4.2 at the level a user sees it. Opening the file and asking for every plan in
    // QUERIES has to leave the data untouched, because a plan is made out of the schema and the
    // directory and a planner that went to the rows to make one would be paying for a query nobody
    // ran yet.
    let path = written("cold");
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("the written file opens");

    let opened = reads(&database);
    assert_eq!(opened.pages, 0, "opening the database read a page of data");
    assert_eq!(opened.indexes, 0, "opening the database read an index");

    let _ = plans(&database);
    let planned = reads(&database);
    assert_eq!(planned, opened, "planning read something out of the file");

    // And running a query does read, which is what makes the two assertions above mean anything
    // rather than being true of a file nobody could read at all.
    // A first scan reads a stripe a part at a time since #1892, so it counts indexes and not pages.
    database.query(QUERIES[0]).expect("the query runs");
    let ran = reads(&database);
    assert!(
        ran.pages > 0 || ran.indexes > 0,
        "the query read nothing, so this file is not being read"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_plan_is_the_same_before_and_after_the_query_has_been_run() {
    // Running a query is the one thing that warms everything there is to warm: the file's page
    // cache, its index cache, the operating system's. A planner that consulted any of them would
    // plan differently the second time, and this is the assertion that says it does not.
    let path = written("warm");
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("the written file opens again");

    let cold = plans(&database);
    for sql in QUERIES {
        database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    let warm = plans(&database);

    assert_eq!(cold, warm, "a plan moved because a query had been run before it");
    let _ = std::fs::remove_file(path);
}

#[test]
fn two_databases_over_one_file_plan_the_same_whatever_either_of_them_did_first() {
    // The same rule from the other side. The second database opens a file whose pages are as warm
    // as the operating system will ever make them, and it has to reach the same plans as the one
    // that opened it cold. Availability of a number is a property of the file, not of the cache.
    let path = written("two");
    let name = path.to_str().expect("a UTF-8 temporary path");

    let first = Database::open(name).expect("the written file opens");
    let cold = plans(&first);
    for sql in QUERIES {
        first.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }

    let second = Database::open(name).expect("the written file opens again");
    assert_eq!(cold, plans(&second), "a second database over the same file reached another plan");
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_setting_is_allowed_to_move_a_plan_and_running_the_query_is_not() {
    // The rule names three things a plan is allowed to be a function of, and a settings change that
    // moved nothing would mean the switch was not wired up rather than that the rule held. The
    // optimizer switch is the coarsest one there is, so it is the one that proves the test can tell
    // two plans apart at all.
    let path = written("settings");
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("the written file opens");

    let optimized = plans(&database);
    database.execute("SET disabled_optimizers = 'filter_pushdown'").expect("a switch is set");
    let without = plans(&database);
    assert_ne!(
        optimized, without,
        "turning an optimizer off moved nothing, so this proves nothing"
    );

    // And with the switch where it was, the plans come back, which means the difference above was
    // the setting rather than the order things happened in.
    database.execute("SET disabled_optimizers = ''").expect("the switch goes back");
    assert_eq!(optimized, plans(&database), "the plans did not come back when the setting did");
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_count_through_a_view_of_a_file_plans_the_same_twice() {
    // The file counts its nulls exactly, so `count(a)` is `count(*)`. Through a view the column is
    // passed on by a projection first, and a rewrite that only saw through it once the projection
    // had been folded away made a plan the second run of the passes changed.
    let path = written("view");
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("the written file opens");
    database.execute("CREATE VIEW v AS SELECT * FROM t").expect("the view is created");
    for sql in ["SELECT count(a) FROM v", "SELECT sum(a), sum(a + 1) FROM v"] {
        let plan = database.plan(sql).unwrap_or_else(|error| panic!("{sql} did not plan: {error}"));
        assert!(plan.contains("count_star()"), "{sql} still counts the column:\n{plan}");
        database.query(sql).unwrap_or_else(|error| panic!("{sql} did not run: {error}"));
    }
    let _ = std::fs::remove_file(path);
}
