//! Transactions on two connections at once: what each one sees, which writes conflict and when,
//! and what the commit makes of two transactions that wrote the same table.
//!
//! The first few follow `conflicts.test` in the compatibility corpus, whose answers were captured
//! on the pin, and so run with `lock_timeout` at zero, which never waits for a held row the way the
//! pin never does. The ones at the end wait.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

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
    waiting("0")
}

/// [`two`] with `lock_timeout` at `timeout`.
fn waiting(timeout: &str) -> (Database, Connection, Connection) {
    let database = Database::new();
    let one = database.connect();
    let two = database.connect();
    one.execute(&format!("SET lock_timeout = '{timeout}'")).expect("sets");
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
fn a_delete_of_a_row_another_transaction_updated_fails_at_once() {
    // The pin lets both commit and loses the update. rudb refuses the second write instead.
    let (_database, one, two) = two();
    one.execute("INSERT INTO t VALUES (3, 30)").expect("inserts");
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 100 WHERE id = 1").expect("updates");
    fails(&two, "DELETE FROM t WHERE id = 1", "Conflict on tuple deletion!");
    one.execute("COMMIT").expect("commits");
    two.execute("ROLLBACK").expect("rolls back");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 100), (2, 20), (3, 30)]));
}

#[test]
fn an_update_of_a_row_another_transaction_deleted_fails_at_once() {
    let (_database, one, two) = two();
    one.execute("BEGIN").expect("begins");
    two.execute("BEGIN").expect("begins");
    one.execute("DELETE FROM t WHERE id = 1").expect("deletes");
    fails(&two, "UPDATE t SET v = 100 WHERE id = 1", "Conflict on update!");
    one.execute("COMMIT").expect("commits");
    two.execute("ROLLBACK").expect("rolls back");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(2, 20)]));
}

#[test]
fn an_update_of_a_row_deleted_after_the_snapshot_fails() {
    let (_database, one, two) = two();
    two.execute("BEGIN").expect("begins");
    assert_eq!(two.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(2));
    one.execute("DELETE FROM t WHERE id = 1").expect("deletes");
    fails(&two, "UPDATE t SET v = 100 WHERE id = 1", "Conflict on update!");
    two.execute("ROLLBACK").expect("rolls back");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(2, 20)]));
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

#[test]
fn lock_timeout_is_a_second_until_set() {
    let database = Database::new();
    let connection = database.connect();
    let setting = || database.setting("lock_timeout").expect("reads");
    assert_eq!(setting(), "1s");
    connection.execute("SET lock_timeout = '250ms'").expect("sets");
    assert_eq!(setting(), "250ms");
    connection.execute("SET lock_timeout = 0").expect("sets");
    assert_eq!(setting(), "0");
    connection.execute("RESET lock_timeout").expect("resets");
    assert_eq!(setting(), "1s");
    fails(&connection, "SET lock_timeout = 'soon'", "lock_timeout is a length of time");
}

#[test]
fn a_write_waits_for_a_holder_that_rolls_back_and_then_goes_ahead() {
    let (_database, one, two) = waiting("10s");
    one.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    two.execute("BEGIN").expect("begins");
    thread::scope(|scope| {
        let waiter = scope.spawn(|| two.execute("UPDATE t SET v = 12 WHERE id = 1"));
        thread::sleep(Duration::from_millis(100));
        one.execute("ROLLBACK").expect("rolls back");
        waiter.join().expect("joins").expect("updates once the row is free");
    });
    two.execute("COMMIT").expect("commits");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 12), (2, 20)]));
}

#[test]
fn a_transaction_that_waited_for_a_holder_that_commits_conflicts() {
    let (_database, one, two) = waiting("10s");
    one.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    two.execute("BEGIN").expect("begins");
    assert_eq!(two.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(2));
    thread::scope(|scope| {
        let waiter = scope.spawn(|| two.execute("UPDATE t SET v = 12 WHERE id = 1"));
        thread::sleep(Duration::from_millis(100));
        one.execute("COMMIT").expect("commits");
        let error = waiter.join().expect("joins").expect_err("the row changed after the snapshot");
        assert!(error.to_string().contains("Conflict on update!"), "{error}");
    });
    two.execute("ROLLBACK").expect("rolls back");
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 11), (2, 20)]));
}

#[test]
fn a_write_outside_a_transaction_waits_for_the_holder_to_commit() {
    let (_database, one, two) = waiting("10s");
    one.execute("BEGIN").expect("begins");
    one.execute("DELETE FROM t WHERE id = 2").expect("deletes");
    thread::scope(|scope| {
        let waiter = scope.spawn(|| two.execute("DELETE FROM t WHERE id >= 2"));
        thread::sleep(Duration::from_millis(100));
        one.execute("INSERT INTO t VALUES (3, 30)").expect("inserts");
        one.execute("COMMIT").expect("commits");
        waiter.join().expect("joins").expect("deletes once the row is free");
    });
    assert_eq!(rows(&one, "SELECT id, v FROM t ORDER BY id"), ints(&[(1, 10)]));
}

#[test]
fn a_wait_gives_up_after_lock_timeout() {
    let (_database, one, two) = waiting("50ms");
    one.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    let started = Instant::now();
    fails(&two, "UPDATE t SET v = 12 WHERE id = 1", "Conflict on update!");
    assert!(started.elapsed() >= Duration::from_millis(50), "{:?}", started.elapsed());
    two.execute("UPDATE t SET v = 21 WHERE id = 2").expect("updates another row");
}

#[test]
fn a_younger_transaction_that_holds_a_row_does_not_wait() {
    let (_database, one, two) = waiting("10s");
    one.execute("BEGIN").expect("begins");
    one.execute("UPDATE t SET v = 11 WHERE id = 1").expect("updates");
    two.execute("BEGIN").expect("begins");
    two.execute("UPDATE t SET v = 22 WHERE id = 2").expect("updates");
    let started = Instant::now();
    fails(&two, "UPDATE t SET v = 12 WHERE id = 1", "Conflict on update!");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}
