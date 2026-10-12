//! A prepared `UPDATE t SET c = c + ? WHERE key = ?` that only adds to its columns is a delta,
//! `08-concurrency.md` section 8.5: two transactions adding to one row both commit, and the row
//! ends with both sums, and a delta and a write of another column of the row both commit too.
//! A write of the column itself still meets the delta. The commit puts the row together from what
//! each wrote, and the log replays to the same row.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rudb::{Connection, Database};
use rudb_common::Value;

const SETUP: &[&str] = &[
    "CREATE TABLE d (id INTEGER PRIMARY KEY, ytd DECIMAL(12, 2), next INTEGER, n INTEGER, \
     name VARCHAR)",
    "INSERT INTO d SELECT i::INTEGER, 30000, 3001, 0, 'd' || i FROM range(100) r(i)",
];

const YTD: &str = "UPDATE d SET ytd = ytd + ? WHERE id = ?";
const COUNT: &str = "UPDATE d SET n = n + 1, ytd = ytd - ? WHERE id = ?";
const NEXT: &str = "UPDATE d SET next = ? WHERE id = ?";
const SET_YTD: &str = "UPDATE d SET ytd = ? WHERE id = ?";

fn cents(cents: i128) -> Value {
    Value::Decimal { unscaled: cents, width: 12, scale: 2 }
}

fn int(value: i32) -> Value {
    Value::Integer(value)
}

fn run(on: &Connection, sql: &str, values: &[Value]) -> rudb_common::Result<()> {
    on.prepare(sql)?.execute(values).map(|_| ())
}

fn row(db: &Connection, id: i32) -> Vec<Value> {
    let sql = format!("SELECT ytd, next, n FROM d WHERE id = {id}");
    db.execute(&sql).expect("reads").rows().next().expect("a row")
}

fn setup(db: &Database) -> (Connection, Connection) {
    for sql in SETUP {
        db.execute(sql).expect(sql);
    }
    let (a, b) = (db.connect(), db.connect());
    for on in [&a, &b] {
        // Short enough that a write that has to wait for the other fails soon.
        on.execute("SET lock_timeout = '50ms'").expect("sets");
    }
    (a, b)
}

/// Two deltas of one row, and a delta with a write of another column of it, in every order of
/// their commits, then a write of the column a delta holds.
fn deltas(a: &Connection, b: &Connection) {
    assert_eq!(a.prepare(YTD).expect("prepares").explain(), "DeltaOne d(id) SET ytd");
    for (first, second) in [(a, b), (b, a)] {
        first.execute("BEGIN").expect("begins");
        second.execute("BEGIN").expect("begins");
        run(first, YTD, &[cents(1050), int(1)]).expect("adds");
        run(second, YTD, &[int(2), int(1)]).expect("adds to the same row");
        run(second, COUNT, &[cents(5), int(1)]).expect("adds again");
        // Each sees its own sums and not the other's.
        assert_eq!(row(first, 1)[0], cents(3_000_000 + 1050));
        assert_eq!(row(second, 1)[0], cents(3_000_000 + 200 - 5));
        first.execute("COMMIT").expect("commits");
        second.execute("COMMIT").expect("commits after the other");
        assert_eq!(row(a, 1), vec![cents(3_000_000 + 1050 + 200 - 5), int(3001), int(1)]);
        run(a, SET_YTD, &[cents(3_000_000), int(1)]).expect("sets");
        run(a, "UPDATE d SET n = 0 WHERE id = ?", &[int(1)]).expect("sets");
    }

    // A delta and a write of another column of the same row.
    for (first, second) in [(a, b), (b, a)] {
        first.execute("BEGIN").expect("begins");
        second.execute("BEGIN").expect("begins");
        run(first, YTD, &[cents(700), int(2)]).expect("adds");
        run(second, NEXT, &[int(3002), int(2)]).expect("writes another column");
        second.execute("COMMIT").expect("commits");
        first.execute("COMMIT").expect("commits after the other");
        assert_eq!(row(a, 2), vec![cents(3_000_700), int(3002), int(0)]);
        run(a, SET_YTD, &[cents(3_000_000), int(2)]).expect("sets");
        run(a, NEXT, &[int(3001), int(2)]).expect("sets");
    }

    // A write committed since the snapshot, of another column, and then a delta.
    a.execute("BEGIN").expect("begins");
    assert_eq!(row(a, 3)[1], int(3001));
    run(b, NEXT, &[int(4000), int(3)]).expect("writes outside a transaction");
    run(a, YTD, &[int(1), int(3)]).expect("adds over the write");
    a.execute("COMMIT").expect("commits");
    assert_eq!(row(b, 3), vec![cents(3_000_100), int(4000), int(0)]);

    // A write of the column the delta holds meets it, and so does the delta a write.
    a.execute("BEGIN").expect("begins");
    run(a, YTD, &[int(1), int(4)]).expect("adds");
    let error = run(b, SET_YTD, &[cents(1), int(4)]).expect_err("meets the delta");
    assert!(error.to_string().contains("Conflict on update!"), "{error}");
    a.execute("COMMIT").expect("commits");
    b.execute("BEGIN").expect("begins");
    run(b, SET_YTD, &[cents(1), int(4)]).expect("sets");
    let error = run(a, YTD, &[int(1), int(4)]).expect_err("meets the write");
    assert!(error.to_string().contains("Conflict on update!"), "{error}");
    b.execute("ROLLBACK").expect("rolls back");
    assert_eq!(row(a, 4)[0], cents(3_000_100));

    // Two deltas whose sums together do not fit the column fail the second commit.
    run(a, "UPDATE d SET n = ? WHERE id = ?", &[int(i32::MAX - 10), int(5)]).expect("sets");
    a.execute("BEGIN").expect("begins");
    b.execute("BEGIN").expect("begins");
    run(a, "UPDATE d SET n = n + 1 WHERE id = ?", &[int(5)]).expect("adds");
    run(a, "UPDATE d SET n = n + 4 WHERE id = ?", &[int(5)]).expect("adds");
    run(b, "UPDATE d SET n = n + 10 WHERE id = ?", &[int(5)]).expect("adds");
    a.execute("COMMIT").expect("commits");
    assert!(b.execute("COMMIT").is_err(), "the sum does not fit");
    assert_eq!(row(a, 5)[2], int(i32::MAX - 5));

    // With waiting off every write of a row fails the way the pin's does, deltas too.
    for on in [a, b] {
        on.execute("SET lock_timeout = '0'").expect("sets");
    }
    a.execute("BEGIN").expect("begins");
    run(a, YTD, &[int(1), int(6)]).expect("adds");
    assert!(run(b, YTD, &[int(1), int(6)]).is_err(), "no deltas without waiting");
    a.execute("COMMIT").expect("commits");
    for on in [a, b] {
        on.execute("SET lock_timeout = '50ms'").expect("sets");
    }
}

/// Every row, in order.
fn all(on: &Connection) -> Vec<Vec<Value>> {
    on.execute("SELECT * FROM d ORDER BY id").expect("reads").rows().collect()
}

#[test]
fn deltas_of_one_row_both_commit() {
    let db = Database::new();
    let (a, b) = setup(&db);
    deltas(&a, &b);
}

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-delta-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("opens")
}

#[test]
fn the_log_replays_deltas_to_the_rows_the_commits_made() {
    let path = path("log");
    let db = open(&path);
    let (a, b) = setup(&db);
    deltas(&a, &b);
    let before = all(&a);
    drop((a, b));
    drop(db);
    let db = open(&path);
    assert_eq!(all(&db.connect()), before);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn many_writers_adding_to_one_row_lose_nothing() {
    const WRITERS: i32 = 4;
    const EACH: i32 = 200;
    let db = Arc::new(Database::new());
    for sql in SETUP {
        db.execute(sql).expect(sql);
    }
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                let on = db.connect();
                let add = on.prepare(COUNT).expect("prepares");
                let next = on.prepare(NEXT).expect("prepares");
                for round in 0..EACH {
                    on.execute("BEGIN").expect("begins");
                    add.execute(&[cents(-1), int(7)]).expect("adds");
                    // A write of another column of the row now and then, on a row of its own.
                    if round % 10 == 0 {
                        next.execute(&[int(round), int(10 + writer)]).expect("writes");
                    }
                    on.execute("COMMIT").expect("commits");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("joins");
    }
    let on = db.connect();
    let total = i128::from(WRITERS * EACH);
    assert_eq!(row(&on, 7), vec![cents(3_000_000 + total), int(3001), int(WRITERS * EACH)]);
}
