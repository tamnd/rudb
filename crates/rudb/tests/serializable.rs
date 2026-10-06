//! `ISOLATION LEVEL SERIALIZABLE`, the one level rudb runs past the pin, which runs snapshot
//! isolation under every level it parses (`engine-v4/18-compat.md` section 18.7).
//!
//! The test of it is the on-call one: two doctors are on call, each sees the other is, and each
//! goes off. Snapshot isolation lets both commit and leaves nobody on call.

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
    let second = bob.execute("COMMIT").is_ok();
    (first, second, count(&alice, on_call))
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
