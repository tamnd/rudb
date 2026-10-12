//! A prepared read of the rows over the first columns of a key or an index, the latest order of a
//! customer and the customers by last name of TPC-C, reads them where the key or the index holds
//! them rather than through the plan. Each check here runs the same statements on a table in its
//! file, checkpointed and opened again, and on one in memory, and compares what the short way
//! answers with what the plan does, through deletes, inserts after them and updates.

use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_common::Value;

const ORDERS: i64 = 40_000;
const CUSTOMERS: i64 = 30_000;

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-front-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("opens")
}

/// Runs a statement on both.
fn both(filed: &Database, memory: &Database, sql: &str) {
    let left = filed.execute(sql).map(|result| result.rows().collect::<Vec<_>>());
    let right = memory.execute(sql).map(|result| result.rows().collect::<Vec<_>>());
    match (left, right) {
        (Ok(left), Ok(right)) => assert_eq!(left, right, "{sql}"),
        (Err(left), Err(right)) => assert_eq!(left.to_string(), right.to_string(), "{sql}"),
        (left, right) => panic!("{sql}: the file said {left:?} and memory {right:?}"),
    }
}

/// Runs `select` with each of `values` the short way on both, and through the plan on the file,
/// which an `OFFSET 0` sends there, and checks they all agree. The short way has to be the one
/// the statement says it takes. How many rows were found.
fn agree(filed: &Database, memory: &Database, select: &str, values: &[Vec<Value>]) -> usize {
    let short = filed.prepare(select).expect("prepares");
    assert!(short.explain().starts_with("Range "), "{select}: {}", short.explain());
    let planned = filed.prepare(&format!("{select} OFFSET 0")).expect("prepares");
    assert_eq!(planned.explain(), "PIPELINE");
    let other = memory.prepare(select).expect("prepares");
    let mut found = 0;
    for values in values {
        let left = short.execute(values).expect(select).rows().collect::<Vec<_>>();
        let plan = planned.execute(values).expect(select).rows().collect::<Vec<_>>();
        let right = other.execute(values).expect(select).rows().collect::<Vec<_>>();
        assert_eq!(left, plan, "{select} {values:?}");
        assert_eq!(left, right, "{select} {values:?}");
        found += left.len();
    }
    found
}

fn int(value: i64) -> Value {
    Value::Integer(i32::try_from(value).expect("a small number"))
}

fn text(value: impl Into<String>) -> Value {
    Value::Varchar(value.into())
}

/// The warehouses and districts every check reads, one of each never there.
fn places() -> Vec<(i64, i64)> {
    let mut places = Vec::new();
    for w in [0, 1, 3, 4] {
        for d in [0, 3, 9, 10] {
            places.push((w, d));
        }
    }
    places
}

fn check(filed: &Database, memory: &Database) {
    let places = places();
    // The latest order of a customer, through the index on the customer.
    let mut by_customer = Vec::new();
    for &(w, d) in &places {
        for c in [-1, 0, 20, 40, 140, 299] {
            by_customer.push(vec![int(w), int(d), int(c)]);
        }
    }
    let latest =
        "SELECT id, carrier FROM o WHERE w = ? AND d = ? AND c = ? ORDER BY id DESC LIMIT 1";
    assert!(agree(filed, memory, latest, &by_customer) > 0);
    let first = "SELECT * FROM o WHERE c = ? AND d = ? AND w = ? ORDER BY id LIMIT 3";
    let flipped: Vec<Vec<Value>> = by_customer
        .iter()
        .map(|values| vec![values[2].clone(), values[1].clone(), values[0].clone()])
        .collect();
    assert!(agree(filed, memory, first, &flipped) > 0);
    let all = "SELECT id FROM o WHERE w = ? AND d = ? AND c = ?";
    assert!(agree(filed, memory, all, &by_customer) > 0);

    // Orders by the front of the key, with a bound on the next column.
    let mut bounded = Vec::new();
    for &(w, d) in &places {
        for id in [-1, 0, 7, 500, 998, 999, 1000] {
            bounded.push(vec![int(w), int(d), int(id)]);
        }
    }
    for select in [
        "SELECT * FROM o WHERE w = ? AND d = ? AND id >= ? ORDER BY id LIMIT 5",
        "SELECT * FROM o WHERE w = ? AND d = ? AND id < ? ORDER BY id DESC LIMIT 5",
        "SELECT id FROM o WHERE w = ? AND d = ? AND id > ? LIMIT 4",
        "SELECT id FROM o WHERE w = ? AND d = ? AND id <= ? ORDER BY id",
    ] {
        agree(filed, memory, select, &bounded);
    }
    let mut districts: Vec<Vec<Value>> =
        places.iter().map(|&(w, d)| vec![int(w), int(d)]).collect();
    agree(
        filed,
        memory,
        "SELECT * FROM o WHERE w = ? AND d = ? ORDER BY id DESC LIMIT 2",
        &districts,
    );

    // Customers by last name, in the order of the first, through an index that is not unique.
    let mut by_name = Vec::new();
    for &(w, d) in &places {
        for last in ["L0", "L5", "L42", "L99", "L100", "M"] {
            by_name.push(vec![int(w), int(d), text(last)]);
        }
    }
    let named =
        "SELECT id, first, balance FROM cu WHERE w = ? AND d = ? AND last = ? ORDER BY first";
    assert!(agree(filed, memory, named, &by_name) > 0);
    let unordered = "SELECT id FROM cu WHERE w = ? AND d = ? AND last = ?";
    assert!(agree(filed, memory, unordered, &by_name) > 0);
    let mut from = Vec::new();
    for values in &by_name {
        let mut values = values.clone();
        values.push(text("f3"));
        from.push(values);
    }
    let after = "SELECT id, first FROM cu WHERE w = ? AND d = ? AND last = ? AND first > ? \
                 ORDER BY first DESC LIMIT 3";
    agree(filed, memory, after, &from);
    // Every name of a district in order, with nulls among them now and then.
    districts.truncate(6);
    agree(
        filed,
        memory,
        "SELECT id, last FROM cu WHERE w = ? AND d = ? ORDER BY last LIMIT 7",
        &districts,
    );
}

/// What prepares the other way: not over the front, or more than the short way reads.
fn refused(filed: &Database) {
    for sql in [
        "SELECT * FROM o WHERE d = ? AND c = ? ORDER BY id LIMIT 1",
        "SELECT * FROM o WHERE w = ? AND d = ? ORDER BY carrier LIMIT 1",
        "SELECT * FROM cu WHERE w = ? AND last = ?",
        "SELECT * FROM cu WHERE w = ? AND d = ? AND first = ?",
    ] {
        assert_eq!(filed.prepare(sql).expect("prepares").explain(), "PIPELINE", "{sql}");
    }
}

#[test]
fn a_read_over_the_front_of_an_index_agrees_with_the_plan() {
    let path = path("front");
    let memory = Database::new();
    let mut filed = open(&path);
    let setup = [
        "CREATE TABLE o (w INTEGER, d INTEGER, id INTEGER, c INTEGER, carrier INTEGER, \
         PRIMARY KEY (w, d, id))"
            .to_owned(),
        "CREATE INDEX o_customer ON o (w, d, c, id)".to_owned(),
        format!(
            "INSERT INTO o SELECT (i % 4)::INTEGER, ((i // 4) % 10)::INTEGER, (i // 40)::INTEGER, \
             ((i * 7) % 300)::INTEGER, CASE WHEN i % 13 = 0 THEN NULL ELSE (i % 11)::INTEGER END \
             FROM range({ORDERS}) r(i)"
        ),
        "CREATE TABLE cu (w INTEGER, d INTEGER, id INTEGER, last VARCHAR, first VARCHAR, \
         balance BIGINT, PRIMARY KEY (w, d, id))"
            .to_owned(),
        format!(
            "INSERT INTO cu SELECT (i % 4)::INTEGER, ((i // 4) % 10)::INTEGER, (i // 40)::INTEGER, \
             CASE WHEN i % 89 = 3 THEN NULL ELSE 'L' || ((i * 13) % 100) END, \
             CASE WHEN i % 97 = 5 THEN NULL ELSE 'f' || (i // 40) END, i \
             FROM range({CUSTOMERS}) r(i)"
        ),
        // After the rows, so the index is built over what is there.
        "CREATE INDEX cu_name ON cu (w, d, last, first)".to_owned(),
    ];
    for sql in &setup {
        both(&filed, &memory, sql);
    }
    check(&filed, &memory);
    refused(&filed);
    filed.execute("CHECKPOINT").expect("checkpoints");
    drop(filed);
    filed = open(&path);
    check(&filed, &memory);

    // Rows gone: one, a few by a range, and a whole name.
    for sql in [
        "DELETE FROM o WHERE w = 1 AND d = 3 AND id = 999",
        "DELETE FROM o WHERE id % 17 = 0",
        "DELETE FROM cu WHERE last = 'L42'",
        "DELETE FROM cu WHERE id % 23 = 1",
    ] {
        both(&filed, &memory, sql);
    }
    check(&filed, &memory);
    // Rows after them, some under keys taken out, updates of a column in no key and of one in an
    // index, which the plan writes.
    for sql in [
        "INSERT INTO o VALUES (1, 3, 999, 20, 1), (1, 3, 1000, 20, 2), (0, 0, 1001, 0, NULL)"
            .to_owned(),
        "INSERT INTO o SELECT (i % 4)::INTEGER, ((i // 4) % 10)::INTEGER, \
         (1002 + i // 40)::INTEGER, ((i * 11) % 300)::INTEGER, 7 FROM range(4000) r(i)"
            .to_owned(),
        "INSERT INTO cu VALUES (0, 0, 5000, 'L42', 'zed', 1), (0, 0, 5001, 'L42', 'abe', 2)"
            .to_owned(),
        "UPDATE o SET carrier = 9 WHERE w = 0 AND d = 0".to_owned(),
        "UPDATE cu SET balance = balance + 1 WHERE w = 1".to_owned(),
        "UPDATE cu SET last = 'L5' WHERE w = 3 AND d = 9 AND id < 100".to_owned(),
        "UPDATE o SET c = 40 WHERE w = 3 AND id % 5 = 0".to_owned(),
    ] {
        both(&filed, &memory, &sql);
    }
    check(&filed, &memory);

    // A transaction reads what it wrote, and the rest do not see it until it commits.
    both(&filed, &memory, "BEGIN");
    both(&filed, &memory, "INSERT INTO o VALUES (3, 9, 5000, 299, 4)");
    both(&filed, &memory, "DELETE FROM cu WHERE w = 0 AND d = 0 AND id = 5001");
    check(&filed, &memory);
    both(&filed, &memory, "COMMIT");
    check(&filed, &memory);

    filed.execute("CHECKPOINT").expect("checkpoints");
    drop(filed);
    filed = open(&path);
    check(&filed, &memory);
    both(&filed, &memory, "DROP INDEX o_customer");
    let latest = "SELECT id FROM o WHERE w = ? AND d = ? AND c = ? ORDER BY id DESC LIMIT 1";
    assert_eq!(filed.prepare(latest).expect("prepares").explain(), "PIPELINE");
    both(&filed, &memory, "CREATE INDEX o_customer ON o (w, d, c, id)");
    check(&filed, &memory);
    drop(filed);
    let _ = std::fs::remove_file(&path);
}
