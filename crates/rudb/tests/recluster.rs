//! A clustered table that rows trickled into out of order is put back in order at a checkpoint,
//! `engine-v4/14-reads-stay-fast.md` section 14.6.
//!
//! Each test loads a table in the order its declaration asks for and then inserts batches of rows
//! from across the whole range, a checkpoint after each, so every batch is a part that a scan of
//! one month has to read. The table is put back in order once those parts are a tenth of its rows,
//! and the answers stay those of the same statements run in memory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rudb::Database;
use rudb_common::Value;

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-recluster-{tag}-{}.rudb", std::process::id()));
    remove(&path);
    path
}

fn wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(wal(path));
}

fn open(path: &Path) -> Database {
    let db = Database::open(path.to_str().expect("a UTF-8 path")).expect("the database opens");
    // One thread, so the rows come back in the order they are stored in.
    db.execute("SET threads = 1").expect("sets");
    db
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = db.query(sql).expect("the query runs");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// The same statements against a database in memory, for what the file one should hold.
fn both(file: &Database, memory: &Database, sql: &str) {
    file.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    memory.execute(sql).unwrap_or_else(|error| panic!("{sql} in memory: {error}"));
}

const SUMMARY: &str = "SELECT count(*), sum(k), min(d), max(d), count(DISTINCT v), sum(length(v)) \
                       FROM t";

fn same(file: &Database, memory: &Database, when: &str) {
    assert_eq!(rows(file, SUMMARY), rows(memory, SUMMARY), "{when}");
}

/// Rows `from` to `to` of four years of dates in an order nothing can rule out, the way
/// `clustered.rs` makes them: 7919 is prime to 1461, so any run of rows covers the whole range.
fn batch(from: u64, to: u64) -> String {
    format!(
        "INSERT INTO t SELECT (r * 7919 % 1000003)::BIGINT, \
         (DATE '2020-01-01' + ((r * 7919) % 1461)::INTEGER)::DATE, 'v' || r \
         FROM range({from}, {to}) AS s(r)"
    )
}

/// A hundred thousand rows loaded in the order `month(d), k` asks for and checkpointed.
fn loaded(file: &Database, memory: &Database) {
    both(file, memory, "CREATE TABLE t (k BIGINT, d DATE, v VARCHAR)");
    file.execute("SET cluster_by = 't(month(d), k)'").expect("declares");
    both(file, memory, &batch(0, 100_000));
    file.execute("CHECKPOINT").expect("checkpoints");
}

/// The month of every row in the order the file holds them.
fn months(db: &Database) -> Vec<i64> {
    rows(db, "SELECT year(d) * 12 + month(d) FROM t")
        .into_iter()
        .map(|row| match row[0] {
            Value::BigInt(month) => month,
            ref other => panic!("a month came back as {other:?}"),
        })
        .collect()
}

fn in_order(months: &[i64]) -> bool {
    months.windows(2).all(|pair| pair[0] <= pair[1])
}

/// How many parts a scan of one month reads, off the scan line of an `EXPLAIN ANALYZE`.
fn parts_read(db: &Database) -> usize {
    let sql = "SELECT count(*) FROM t WHERE d >= DATE '2021-06-01' AND d < DATE '2021-07-01'";
    let result = db.query(&format!("EXPLAIN ANALYZE {sql}")).expect("the explain runs");
    let Value::Varchar(text) = result.value_at(0, 1) else { panic!("the plan is text") };
    let line = text.lines().find(|line| line.contains("Get ")).expect("a scan on the tree");
    let Some(at) = line.find(" parts skipped") else { return usize::MAX };
    let words = line[..at].rsplit(", ").next().expect("the clause");
    let numbers = words
        .split(" of ")
        .map(|word| word.trim().parse::<usize>().expect("a count"))
        .collect::<Vec<_>>();
    numbers[1] - numbers[0]
}

#[test]
fn a_table_is_put_back_in_order_once_the_rows_out_of_place_are_a_tenth_of_it() {
    let path = path("tenth");
    let (file, memory) = (open(&path), Database::new());
    assert_eq!(file.setting("recluster_depth").expect("a setting"), "2");
    loaded(&file, &memory);
    assert!(in_order(&months(&file)), "the load is in order");
    let fresh = parts_read(&file);

    // Each batch of a thousand rows is a part in the file once a checkpoint has written it, and the
    // checkpoint after the batch that takes them to a tenth of the rows puts the table in order. A
    // batch can share a part with the end of the load, so the batch it happens at is pinned to a
    // range rather than to one.
    let mut decayed = fresh;
    let mut at = None;
    for round in 1..=16 {
        let from = 99_000 + round * 1000;
        both(&file, &memory, &batch(from, from + 1000));
        file.execute("CHECKPOINT").expect("checkpoints");
        if in_order(&months(&file)) {
            at = Some(round);
            break;
        }
        decayed = parts_read(&file);
    }
    let at = at.expect("sixteen batches put the table back in order");
    assert!((11..=13).contains(&at), "the table was put in order after batch {at}");
    assert!(decayed >= fresh + 9, "a month read every batch: {fresh} then {decayed}");
    let after = parts_read(&file);
    assert!(after <= fresh + 1, "a month reads what it did after the load: {fresh} then {after}");
    same(&file, &memory, "put back in order");

    drop(file);
    let file = open(&path);
    assert!(in_order(&months(&file)), "the order is in the file");
    same(&file, &memory, "after a reopen");

    // A change by row number after it lands on the right rows, in the file and after a reopen.
    both(&file, &memory, "UPDATE t SET v = v || '!' WHERE k % 97 = 3");
    both(&file, &memory, "DELETE FROM t WHERE k % 89 = 5");
    same(&file, &memory, "changed after");
    file.execute("CHECKPOINT").expect("checkpoints");
    drop(file);
    let file = open(&path);
    same(&file, &memory, "changed after a reopen");
    drop(file);
    remove(&path);
}

#[test]
fn changes_logged_after_a_table_is_put_back_in_order_survive_a_crash() {
    let path = path("crash");
    let (file, memory) = (open(&path), Database::new());
    loaded(&file, &memory);
    for round in 0..16 {
        let from = 100_000 + round * 1000;
        both(&file, &memory, &batch(from, from + 1000));
        file.execute("CHECKPOINT").expect("checkpoints");
        if in_order(&months(&file)) {
            break;
        }
    }
    assert!(in_order(&months(&file)), "the table was put back in order");
    // Each of these is logged by the numbers the rows have now, and the next open replays them
    // over the file that holds the rows in that order.
    both(&file, &memory, "UPDATE t SET k = k + 1 WHERE d < DATE '2020-03-01'");
    both(&file, &memory, "DELETE FROM t WHERE k % 13 = 0");
    both(&file, &memory, &batch(200_000, 200_500));
    same(&file, &memory, "before the crash");
    std::mem::forget(file);

    let file = open(&path);
    same(&file, &memory, "after the crash");
    drop(file);
    remove(&path);
}

#[test]
fn a_depth_of_zero_never_reorders_and_a_depth_under_one_is_refused() {
    let path = path("off");
    let (file, memory) = (open(&path), Database::new());
    for wrong in ["0.5", "-1", "'deep'"] {
        let error = file.execute(&format!("SET recluster_depth = {wrong}")).expect_err(wrong);
        assert!(error.to_string().contains("recluster_depth is 0"), "{wrong}: {error}");
    }
    file.execute("SET recluster_depth = 0").expect("sets");
    assert_eq!(file.setting("recluster_depth").expect("a setting"), "0");
    loaded(&file, &memory);
    for round in 0..20 {
        let from = 100_000 + round * 1000;
        both(&file, &memory, &batch(from, from + 1000));
        file.execute("CHECKPOINT").expect("checkpoints");
    }
    assert!(!in_order(&months(&file)), "nothing put the table back in order");
    same(&file, &memory, "never reordered");

    // Turned back on, the next checkpoint does it.
    file.execute("RESET recluster_depth").expect("resets");
    file.execute("INSERT INTO t VALUES (-1, DATE '2022-02-02', 'last')").expect("inserts");
    memory.execute("INSERT INTO t VALUES (-1, DATE '2022-02-02', 'last')").expect("inserts");
    file.execute("CHECKPOINT").expect("checkpoints");
    assert!(in_order(&months(&file)), "the checkpoint after the reset put it in order");
    same(&file, &memory, "reordered at last");
    drop(file);
    remove(&path);
}

/// Without a `CHECKPOINT` anywhere: the checkpoints a commit starts on a thread of its own write
/// the batches, and the commit that finds the table decayed writes it in order before it returns.
#[test]
fn checkpoints_off_the_commit_path_leave_the_reordering_to_a_commit() {
    let read = |depth: &str| {
        let path = path(&format!("background-{depth}"));
        let (file, memory) = (open(&path), Database::new());
        file.execute(&format!("SET recluster_depth = {depth}")).expect("sets");
        loaded(&file, &memory);
        file.execute("SET checkpoint_threshold = '64KiB'").expect("sets");
        for round in 0..40 {
            let from = 100_000 + round * 1000;
            both(&file, &memory, &batch(from, from + 1000));
        }
        std::thread::sleep(Duration::from_millis(500));
        same(&file, &memory, "while open");
        let parts = parts_read(&file);
        drop(file);
        let file = open(&path);
        same(&file, &memory, "after a reopen");
        drop(file);
        remove(&path);
        parts
    };
    let (on, off) = (read("2"), read("0"));
    assert!(on < off, "a month read {on} parts with the reclusterer and {off} without it");
}
