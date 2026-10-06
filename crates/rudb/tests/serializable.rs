//! `ISOLATION LEVEL SERIALIZABLE`, the one level rudb runs past the pin, which runs snapshot
//! isolation under every level it parses (`engine-v4/18-compat.md` section 18.7).
//!
//! The test of it is the on-call one: two doctors are on call, each sees the other is, and each
//! goes off. Snapshot isolation lets both commit and leaves nobody on call. Serializable fails the
//! second commit, because the second doctor read a row the first one changed.

use rudb::{Connection, Database};
use rudb_common::Value;

fn count(connection: &Connection, sql: &str) -> Value {
    let result = connection.query(sql).expect("the query runs");
    result.value_at(0, 0)
}

/// A database with two doctors on call and a connection for each.
fn doctors() -> (Database, Connection, Connection) {
    let database = Database::new();
    let alice = database.connect();
    let bob = database.connect();
    alice.execute("CREATE TABLE doctors (name VARCHAR, on_call BOOLEAN)").expect("creates");
    alice.execute("INSERT INTO doctors VALUES ('alice', true), ('bob', true)").expect("inserts");
    (database, alice, bob)
}

/// Each doctor checks that someone else is on call and goes off, both inside transactions begun
/// with `begin`, and says whether each commit went through.
fn go_off(begin: &str) -> (bool, bool, Value) {
    let (_database, alice, bob) = doctors();
    alice.execute(begin).expect("begins");
    bob.execute(begin).expect("begins");
    let on_call = "SELECT count(*) FROM doctors WHERE on_call";
    assert_eq!(count(&alice, on_call), Value::BigInt(2));
    assert_eq!(count(&bob, on_call), Value::BigInt(2));
    alice.execute("UPDATE doctors SET on_call = false WHERE name = 'alice'").expect("updates");
    bob.execute("UPDATE doctors SET on_call = false WHERE name = 'bob'").expect("updates");
    let first = alice.execute("COMMIT").is_ok();
    let second = match bob.execute("COMMIT") {
        Ok(_) => true,
        Err(error) => {
            assert!(error.to_string().contains("could not serialize access"), "{error}");
            false
        }
    };
    (first, second, count(&alice, on_call))
}

#[test]
fn at_serializable_the_second_doctor_cannot_go_off() {
    for begin in [
        "BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "START TRANSACTION READ WRITE, ISOLATION LEVEL SERIALIZABLE",
    ] {
        assert_eq!(go_off(begin), (true, false, Value::BigInt(1)), "{begin}");
    }
}

#[test]
fn serializable_transactions_on_rows_apart_both_commit() {
    let database = Database::new();
    let one = database.connect();
    let two = database.connect();
    one.execute("CREATE TABLE accounts (id INTEGER, balance INTEGER)").expect("creates");
    one.execute("INSERT INTO accounts VALUES (1, 10), (2, 20)").expect("inserts");
    for connection in [&one, &two] {
        connection.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").expect("begins");
    }
    assert_eq!(count(&one, "SELECT balance FROM accounts WHERE id = 1"), Value::Integer(10));
    assert_eq!(count(&two, "SELECT balance FROM accounts WHERE id = 2"), Value::Integer(20));
    one.execute("UPDATE accounts SET balance = balance + 1 WHERE id = 1").expect("updates");
    two.execute("UPDATE accounts SET balance = balance + 2 WHERE id = 2").expect("updates");
    one.execute("COMMIT").expect("the first commits");
    two.execute("COMMIT").expect("the second read nothing the first changed");
    assert_eq!(count(&one, "SELECT sum(balance)::BIGINT FROM accounts"), Value::BigInt(33));
}

#[test]
fn a_row_added_that_a_read_picks_fails_the_commit_and_one_it_does_not_pick_does_not() {
    let (_database, alice, bob) = doctors();
    let on_call = "SELECT count(*) FROM doctors WHERE on_call";
    alice.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").expect("begins");
    assert_eq!(count(&alice, on_call), Value::BigInt(2));
    bob.execute("INSERT INTO doctors VALUES ('dave', false)").expect("inserts");
    alice.execute("UPDATE doctors SET on_call = false WHERE name = 'alice'").expect("updates");
    alice.execute("COMMIT").expect("dave is not on call, so the count still holds");

    alice.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").expect("begins");
    assert_eq!(count(&alice, on_call), Value::BigInt(1));
    bob.execute("INSERT INTO doctors VALUES ('carol', true)").expect("inserts");
    alice.execute("INSERT INTO doctors VALUES ('erin', false)").expect("inserts");
    let error = alice.execute("COMMIT").expect_err("carol is a phantom the count missed");
    assert!(error.to_string().contains("could not serialize access"), "{error}");
    assert_eq!(count(&alice, "SELECT count(*) FROM doctors"), Value::BigInt(4));
}

#[test]
fn every_level_but_serializable_lets_both_doctors_go_off() {
    for begin in [
        "BEGIN",
        "BEGIN TRANSACTION",
        "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "START TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "BEGIN ISOLATION LEVEL READ UNCOMMITTED, READ WRITE",
    ] {
        assert_eq!(go_off(begin), (true, true, Value::BigInt(0)), "{begin}");
    }
}

#[test]
fn a_serializable_transaction_reads_and_writes_as_any_other_alone() {
    let (_database, alice, _bob) = doctors();
    alice.execute("BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE").expect("begins");
    alice.execute("UPDATE doctors SET on_call = false WHERE name = 'alice'").expect("updates");
    assert_eq!(count(&alice, "SELECT count(*) FROM doctors WHERE on_call"), Value::BigInt(1));
    alice.execute("COMMIT").expect("commits");
    alice.execute("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY").expect("begins");
    let refused = alice.execute("DELETE FROM doctors").expect_err("read only");
    assert!(refused.to_string().contains("read-only mode"), "{refused}");
    alice.execute("ROLLBACK").expect("rolls back");
}

/// Two accounts by key, both connections in serializable transactions, and the read and the write
/// by key prepared on each so they take the point path.
fn keyed() -> (Database, Connection, Connection) {
    let database = Database::new();
    let one = database.connect();
    let two = database.connect();
    one.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT)").expect("creates");
    one.execute("INSERT INTO accounts VALUES (1, 10), (2, 20)").expect("inserts");
    for connection in [&one, &two] {
        connection.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").expect("begins");
    }
    (database, one, two)
}

/// Reads the balance of `id` by its key, on the point path, `None` when no row holds it.
fn balance(connection: &Connection, id: i64) -> Option<i64> {
    let read = connection.prepare("SELECT balance FROM accounts WHERE id = ?").expect("prepares");
    assert_ne!(read.explain(), "PIPELINE", "a read by key at serializable takes the point path");
    let result = read.execute(&[Value::BigInt(id)]).expect("reads");
    (!result.is_empty()).then(|| match result.value_at(0, 0) {
        Value::BigInt(balance) => balance,
        other => panic!("{other:?}"),
    })
}

/// Adds `by` to the balance of `id` by its key, on the point path.
fn add(connection: &Connection, id: i64, by: i64) {
    let write = connection
        .prepare("UPDATE accounts SET balance = balance + ? WHERE id = ?")
        .expect("prepares");
    assert_ne!(write.explain(), "PIPELINE", "a write by key at serializable takes the point path");
    write.execute(&[Value::BigInt(by), Value::BigInt(id)]).expect("updates");
}

#[test]
fn reads_and_writes_by_key_on_keys_apart_both_commit() {
    let (_database, one, two) = keyed();
    assert_eq!(balance(&one, 1), Some(10));
    assert_eq!(balance(&two, 2), Some(20));
    add(&one, 1, 1);
    add(&two, 2, 2);
    one.execute("COMMIT").expect("the first commits");
    two.execute("COMMIT").expect("the second read no key the first wrote");
    assert_eq!(count(&one, "SELECT sum(balance)::BIGINT FROM accounts"), Value::BigInt(33));
}

#[test]
fn a_key_read_that_another_commit_wrote_fails_the_commit() {
    let (_database, one, two) = keyed();
    // Each makes sure the two balances add up to enough, then takes from its own: write skew.
    for connection in [&one, &two] {
        assert_eq!(balance(connection, 1).zip(balance(connection, 2)), Some((10, 20)));
    }
    add(&one, 1, -25);
    add(&two, 2, -25);
    one.execute("COMMIT").expect("the first commits");
    let error = two.execute("COMMIT").expect_err("the second read the key the first wrote");
    assert!(error.to_string().contains("could not serialize access"), "{error}");
    assert_eq!(count(&one, "SELECT sum(balance)::BIGINT FROM accounts"), Value::BigInt(5));
}

#[test]
fn a_key_read_and_found_missing_fails_the_commit_when_another_adds_it() {
    let (_database, one, two) = keyed();
    assert_eq!(balance(&one, 3), None);
    add(&one, 1, 1);
    two.execute("INSERT INTO accounts VALUES (3, 30)").expect("inserts");
    two.execute("COMMIT").expect("commits");
    let error = one.execute("COMMIT").expect_err("the key it found missing is there now");
    assert!(error.to_string().contains("could not serialize access"), "{error}");
}

#[test]
fn a_serializable_transaction_that_wrote_nothing_always_commits() {
    let (_database, one, two) = keyed();
    assert_eq!(balance(&one, 1), Some(10));
    assert_eq!(count(&one, "SELECT sum(balance)::BIGINT FROM accounts"), Value::BigInt(30));
    add(&two, 1, 5);
    two.execute("COMMIT").expect("commits");
    one.execute("COMMIT").expect("what it read is what was there at its snapshot");
}
