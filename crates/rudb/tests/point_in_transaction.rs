//! A prepared read by key or by a short range of keys takes the short way inside a transaction
//! too, and has to see what the plan sees there: the transaction's own writes and nothing another
//! connection committed after the snapshot. Each check here runs the statement and the same
//! statement in a shape the short way leaves to the plan on the same connection, and compares
//! the rows.

use rudb::{Connection, Database};
use rudb_common::Value;

fn rows(result: &rudb::QueryResult) -> Vec<Vec<Value>> {
    result.rows().collect()
}

const BY_ID: &str = "SELECT * FROM t WHERE id = ?";
const FROM_ID: &str = "SELECT * FROM t WHERE id >= ? ORDER BY id LIMIT ?";
const BY_NAME: &str = "SELECT id, n FROM t WHERE name = ?";

/// Reads `id` by key and the five rows from it on, both the short way and through the plan on
/// `db`, checks they agree, and hands back the row of `id`.
fn read(db: &Connection, id: i64) -> Vec<Vec<Value>> {
    let key = [Value::BigInt(id)];
    let found = rows(&db.prepare(BY_ID).expect("prepares").execute(&key).expect("reads"));
    let planned = db.prepare(&format!("{BY_ID} LIMIT 1000")).expect("prepares");
    assert_eq!(found, rows(&planned.execute(&key).expect("reads")), "{id}");

    let range = [Value::BigInt(id), Value::BigInt(5)];
    let short = rows(&db.prepare(FROM_ID).expect("prepares").execute(&range).expect("reads"));
    let planned = "SELECT * FROM t WHERE id >= ? ORDER BY id LIMIT 5000";
    let mut planned = rows(&db.prepare(planned).expect("prepares").execute(&key).expect("reads"));
    planned.truncate(5);
    assert_eq!(short, planned, "from {id}");

    let name = [Value::Varchar(format!("n{id}"))];
    let short = rows(&db.prepare(BY_NAME).expect("prepares").execute(&name).expect("reads"));
    let planned = db.prepare(&format!("{BY_NAME} LIMIT 1000")).expect("prepares");
    assert_eq!(short, rows(&planned.execute(&name).expect("reads")), "n{id}");
    found
}

/// A database with the table, and a connection to it.
fn setup() -> (Database, Connection) {
    let db = Database::new();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR UNIQUE, n BIGINT)")
        .expect("creates");
    db.execute("INSERT INTO t SELECT i, 'n' || i, i * 10 FROM range(4000) r(i)").expect("loads");
    let connection = db.connect();
    (db, connection)
}

fn named(db: &Connection, sql: &str) -> String {
    db.prepare(sql).expect("prepares").explain()
}

#[test]
fn a_read_inside_a_transaction_sees_the_transaction_and_not_what_came_after() {
    let (base, db) = setup();
    let mine = base.connect();
    // Read once, so what the lookups build for the committed rows is there to be shared.
    read(&db, 5);
    mine.execute("BEGIN").expect("begins");
    assert_eq!(named(&mine, BY_ID), "POINT Lookup t(id)");
    assert_eq!(named(&mine, FROM_ID), "Range t(id)");
    assert_eq!(read(&mine, 7).len(), 1);

    // Another connection writes after the snapshot, which the transaction does not see.
    db.execute("UPDATE t SET n = -1 WHERE id = 7").expect("updates");
    db.execute("DELETE FROM t WHERE id = 8").expect("deletes");
    db.execute("INSERT INTO t VALUES (9000, 'n9000', 1), (6, 'other', 0)").expect_err("a dup");
    db.execute("INSERT INTO t VALUES (9000, 'n9000', 1)").expect("inserts");
    assert_eq!(read(&mine, 7)[0][2], Value::BigInt(70));
    assert_eq!(read(&mine, 8).len(), 1);
    assert!(read(&mine, 9000).is_empty());
    assert_eq!(read(&db, 7)[0][2], Value::BigInt(-1));
    assert!(read(&db, 8).is_empty());
    assert_eq!(read(&db, 9000).len(), 1);
    mine.execute("ROLLBACK").expect("rolls back");
    for id in [6, 7, 8, 9, 9000] {
        assert_eq!(read(&mine, id), read(&db, id), "{id}");
    }

    // The transaction's own writes, by the plan and by the short way, which it does see.
    mine.execute("BEGIN").expect("begins");
    mine.execute("UPDATE t SET n = n + 1 WHERE id = 10").expect("updates");
    mine.execute("DELETE FROM t WHERE id = 11").expect("deletes");
    let insert = mine.prepare("INSERT INTO t VALUES (?, ?, ?)").expect("prepares");
    for id in [5000, 5001, 4000] {
        let row = [Value::BigInt(id), Value::Varchar(format!("n{id}")), Value::BigInt(id)];
        insert.execute(&row).expect("inserts");
    }
    for id in [9, 10, 11, 12, 3999, 4000, 5000, 5001, 9000] {
        read(&mine, id);
    }
    assert_eq!(read(&mine, 10)[0][2], Value::BigInt(101));
    assert!(read(&mine, 11).is_empty());
    assert_eq!(read(&mine, 5001).len(), 1);
    assert!(read(&db, 5001).is_empty());
    assert_eq!(read(&db, 10)[0][2], Value::BigInt(100));

    mine.execute("COMMIT").expect("commits");
    for id in [7, 8, 10, 11, 4000, 5000, 5001, 9000] {
        assert_eq!(read(&mine, id), read(&db, id), "{id}");
    }
    assert_eq!(read(&db, 10)[0][2], Value::BigInt(101));
    assert!(read(&db, 11).is_empty());
}

#[test]
fn a_read_after_a_rollback_or_an_abort_is_the_committed_one() {
    let (_base, db) = setup();
    db.execute("BEGIN").expect("begins");
    db.execute("DELETE FROM t WHERE id < 100").expect("deletes");
    db.execute("UPDATE t SET name = 'gone' || id WHERE id >= 3000").expect("updates");
    assert!(read(&db, 50).is_empty());
    assert_eq!(read(&db, 3500)[0][1], Value::Varchar("gone3500".into()));
    db.execute("ROLLBACK").expect("rolls back");
    assert_eq!(read(&db, 50).len(), 1);
    assert_eq!(read(&db, 3500)[0][1], Value::Varchar("n3500".into()));

    // An aborted transaction refuses the read the short way as the plan does.
    let by_id = db.prepare(BY_ID).expect("prepares");
    let range = db.prepare(FROM_ID).expect("prepares");
    db.execute("BEGIN").expect("begins");
    db.execute("INSERT INTO t VALUES (1, 'x', 0)").expect_err("a duplicate");
    assert_eq!(by_id.explain(), "PIPELINE");
    assert_eq!(range.explain(), "PIPELINE");
    let refused = by_id.execute(&[Value::BigInt(1)]).expect_err("aborted").to_string();
    assert!(refused.contains("Current transaction is aborted"), "{refused}");
    range.execute(&[Value::BigInt(1), Value::BigInt(5)]).expect_err("aborted");
    db.execute("ROLLBACK").expect("rolls back");
    assert_eq!(read(&db, 1).len(), 1);

    // And a read only transaction reads the short way.
    db.execute("BEGIN TRANSACTION READ ONLY").expect("begins");
    assert_eq!(named(&db, BY_ID), "POINT Lookup t(id)");
    assert_eq!(read(&db, 1).len(), 1);
    db.execute("COMMIT").expect("commits");
}

#[test]
fn many_transactions_reading_and_writing_agree_with_the_plan() {
    let (base, db) = setup();
    let others = base.connect();
    for round in 0..60_i64 {
        db.execute("BEGIN").expect("begins");
        let id = (round * 37) % 4100;
        read(&db, id);
        db.execute(&format!("UPDATE t SET n = n + 1 WHERE id = {id}")).expect("updates");
        // Another connection's insert after the snapshot, which the update commits beside, and
        // the delete does not.
        if round % 4 == 0 {
            others
                .execute(&format!("INSERT INTO t VALUES ({}, 'o{round}', 0)", 10_000 + round))
                .expect("inserts");
        }
        if round % 5 == 0 {
            db.execute(&format!("DELETE FROM t WHERE id = {}", id + 1)).expect("deletes");
        }
        read(&db, id);
        read(&db, id + 1);
        read(&db, 10_000 + round);
        if round % 3 == 0 {
            db.execute("ROLLBACK").expect("rolls back");
        } else if round % 20 == 0 {
            db.execute("COMMIT").expect_err("a delete beside rows appended since");
        } else {
            db.execute("COMMIT").expect("commits");
        }
        read(&db, id);
    }
    let all = "SELECT count(*), sum(n) FROM t";
    let mine = rows(&db.execute(all).expect("reads"));
    assert_eq!(mine, rows(&others.execute(all).expect("reads")));
}
