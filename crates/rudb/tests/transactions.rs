//! Transactions on two connections at once: what each one sees, which writes conflict and when,
//! and what the commit makes of two transactions that wrote the same table.
//!
//! The first few follow `conflicts.test` in the compatibility corpus, whose answers were captured
//! on the pin.

use std::path::{Path, PathBuf};

use rudb::{Connection, Database};
use rudb_common::Value;

fn rows(connection: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let result = connection.query(sql).expect("the query runs");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

fn ints(pairs: &[(i32, i32)]) -> Vec<Vec<Value>> {
    pairs.iter().map(|&(a, b)| vec![Value::Integer(a), Value::Integer(b)]).collect()
}

fn fails(connection: &Connection, sql: &str, text: &str) {
    let error = connection.execute(sql).expect_err("the statement fails");
    assert!(error.to_string().contains(text), "{sql}: {error}");
}

fn two() -> (Database, Connection, Connection) {
    let database = Database::new();
    let one = database.connect();
    let two = database.connect();
    one.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").expect("creates");
    one.execute("INSERT INTO t VALUES (1, 10), (2, 20)").expect("inserts");
    (database, one, two)
}

#[test]
fn a_second_update_of_one_row_fails_at_once() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    fails(&two, "UPDATE t SET v = 12 WHERE id = 1", "Conflict on update!");
    one.execute("COMMIT").expect("commits");
    two.execute("ROLLBACK").expect("rolls back");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 11), (2, 20)]));
}

#[test]
fn a_key_both_inserted_fails_at_the_second_commit() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("INSERT INTO t VALUES (3, 30)").expect("inserts");
    two.execute("INSERT INTO t VALUES (3, 31)").expect("inserts");
    one.execute("COMMIT").expect("commits");
    fails(
        &two,
        "COMMIT",
        "Failed to commit: PRIMARY KEY or UNIQUE constraint violation: duplicate key \"3\"",
    );
    assert_eq!(rows(&two, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 10), (2, 20), (3, 30)]));
}

#[test]
fn a_second_delete_of_one_row_fails_at_once() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("DELETE FROM t WHERE id = 2").expect("deletes");
    fails(&two, "DELETE FROM t WHERE id = 2", "Conflict on tuple deletion!");
    one.execute("COMMIT").expect("commits");
    two.execute("ROLLBACK").expect("rolls back");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 10)]));
}

#[test]
fn a_second_create_of_one_name_fails_at_once() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("CREATE TABLE u (a INTEGER)").expect("creates");
    fails(&two, "CREATE TABLE u (a INTEGER)", "Catalog write-write conflict on create with");
    one.execute("COMMIT").expect("commits");
    two.execute("ROLLBACK").expect("rolls back");
    assert_eq!(two.value("SELECT count(*) FROM u").expect("reads"), Value::BigInt(0));
}

#[test]
fn an_update_and_a_delete_of_one_row_both_commit_and_the_last_wins() {
    let (_database, one, two) = two();
    one.execute("INSERT INTO t VALUES (3, 30)").expect("inserts");
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 100 WHERE id = 1").expect("updates");
    two.execute("DELETE FROM t WHERE id = 1").expect("deletes");
    one.execute("COMMIT").expect("commits");
    two.execute("COMMIT").expect("commits");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(2, 20), (3, 30)]));
}

#[test]
fn uncommitted_rows_are_seen_only_by_their_own_connection() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    one.execute("INSERT INTO t VALUES (4, 40)").expect("inserts");
    assert_eq!(one.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(3));
    assert_eq!(two.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(2));
    one.execute("ROLLBACK").expect("rolls back");
    assert_eq!(one.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(2));
}

#[test]
fn a_transaction_does_not_see_what_was_committed_after_its_snapshot() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    assert_eq!(one.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(2));
    two.execute("INSERT INTO t VALUES (5, 50)").expect("inserts");
    assert_eq!(one.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(2));
    one.execute("COMMIT").expect("commits");
    assert_eq!(one.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(3));
}

#[test]
fn updates_of_different_rows_of_one_table_both_commit() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    two.execute("UPDATE t SET v = 22 WHERE id = 2").expect("updates");
    two.execute("INSERT INTO t VALUES (6, 60)").expect("inserts");
    one.execute("COMMIT").expect("commits");
    two.execute("COMMIT").expect("commits");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 11), (2, 22), (6, 60)]));
}

#[test]
fn appends_to_one_table_from_two_transactions_both_commit() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("INSERT INTO t VALUES (7, 70)").expect("inserts");
    two.execute("INSERT INTO t VALUES (8, 80)").expect("inserts");
    two.execute("COMMIT").expect("commits");
    one.execute("COMMIT").expect("commits");
    assert_eq!(one.value("SELECT sum(id) FROM t").expect("reads"), Value::HugeInt(18));
}

#[test]
fn transactions_on_different_tables_both_commit_even_with_a_new_table_in_one() {
    let (_database, one, two) = two();
    one.execute("CREATE TABLE s (a INTEGER)").expect("creates");
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("INSERT INTO s VALUES (1)").expect("inserts");
    two.execute("CREATE TABLE w (b INTEGER)").expect("creates");
    two.execute("INSERT INTO w VALUES (2)").expect("inserts");
    one.execute("COMMIT").expect("commits");
    two.execute("COMMIT").expect("commits");
    assert_eq!(one.value("SELECT count(*) FROM s").expect("reads"), Value::BigInt(1));
    assert_eq!(one.value("SELECT count(*) FROM w").expect("reads"), Value::BigInt(1));
}

#[test]
fn a_write_outside_a_transaction_conflicts_with_a_row_one_holds() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    fails(&two, "UPDATE t SET v = 12 WHERE id = 1", "Conflict on update!");
    two.execute("UPDATE t SET v = 21 WHERE id = 2").expect("updates another row");
    one.execute("COMMIT").expect("commits");
    assert_eq!(rows(&two, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 11), (2, 21)]));
}

#[test]
fn a_connection_dropped_in_a_transaction_frees_its_rows() {
    let (database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    drop(one);
    two.execute("UPDATE t SET v = 12 WHERE id = 1").expect("updates");
    let three = database.connect();
    assert_eq!(rows(&three, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 12), (2, 20)]));
}

#[test]
fn checkpoint_in_a_transaction_that_wrote_is_refused() {
    let (_database, one, _two) = two();
    one.execute("BEGIN").expect("begins");
    one.execute("CHECKPOINT").expect("nothing written yet");
    one.execute("INSERT INTO t VALUES (9, 90)").expect("inserts");
    fails(
        &one,
        "CHECKPOINT",
        "Cannot CHECKPOINT: the current transaction has transaction local changes",
    );
}

fn file(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-txn-{tag}-{}.rudb", std::process::id()));
    remove(&path);
    path
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
}

#[test]
fn two_transactions_that_committed_survive_a_crash() {
    let path = file("crash");
    let database = Database::open(path.to_str().expect("UTF-8")).expect("opens");
    let one = database.connect();
    let two = database.connect();
    one.execute("CREATE TABLE t (id INTEGER, v INTEGER)").expect("creates");
    one.execute("INSERT INTO t VALUES (1, 10), (2, 20)").expect("inserts");
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    two.execute("INSERT INTO t VALUES (3, 30)").expect("inserts");
    two.execute("DELETE FROM t WHERE id = 2").expect("deletes");
    one.execute("COMMIT").expect("commits");
    two.execute("COMMIT").expect("commits");
    let open = database.connect();
    open.execute("BEGIN").expect("begins");
    open.execute("INSERT INTO t VALUES (4, 40)").expect("inserts");
    drop((one, two));
    std::mem::forget(open);
    std::mem::forget(database);

    let database = Database::open(path.to_str().expect("UTF-8")).expect("reopens");
    let connection = database.connect();
    assert_eq!(rows(&connection, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 11), (3, 30)]));
    drop(connection);
    drop(database);
    remove(&path);
}
