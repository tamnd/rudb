//! A prepared `SELECT ... WHERE key = ?` finds its row by the key rather than through the plan,
//! and has to answer exactly what the plan would. Each check here runs the same values through
//! the statement, which can take the short way, and through the same statement with a `LIMIT`
//! after it, which cannot, and compares the names, the types, the rows and the errors.

use std::path::PathBuf;

use rudb::Database;
use rudb_common::Value;

fn rows(result: &rudb::QueryResult) -> Vec<Vec<Value>> {
    result.rows().collect()
}

/// Runs `values` through `select` and through `select` with a `LIMIT`, and checks they agree.
fn agree(db: &Database, select: &str, values: &[Value]) -> Vec<Vec<Value>> {
    let short = db.prepare(select).expect("prepares").execute(values);
    let planned = db.prepare(&format!("{select} LIMIT 1000")).expect("prepares").execute(values);
    match (short, planned) {
        (Ok(short), Ok(planned)) => {
            assert_eq!(short.names(), planned.names(), "{select} {values:?}");
            assert_eq!(short.types(), planned.types(), "{select} {values:?}");
            assert_eq!(rows(&short), rows(&planned), "{select} {values:?}");
            rows(&short)
        }
        (Err(short), Err(planned)) => {
            assert_eq!(short.to_string(), planned.to_string(), "{select} {values:?}");
            Vec::new()
        }
        (short, planned) => {
            panic!("{select} {values:?}: the short way said {short:?} and the plan {planned:?}")
        }
    }
}

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-point-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

#[test]
fn a_key_finds_its_row_the_way_the_plan_does() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR UNIQUE, n BIGINT, d DOUBLE)")
        .expect("creates");
    db.execute(
        "INSERT INTO t SELECT i::INTEGER, 'n' || i, i * 10, i / 4 FROM range(5000) r(i) \
         WHERE i % 7 <> 3",
    )
    .expect("loads");
    db.execute("INSERT INTO t VALUES (-1, NULL, NULL, NULL)").expect("inserts");

    let by_id = "SELECT * FROM t WHERE id = ?";
    for id in [0, 1, 3, 2047, 2048, 4999, 5000, -1, -2] {
        agree(&db, by_id, &[Value::Integer(id)]);
        agree(&db, by_id, &[Value::BigInt(i64::from(id))]);
    }
    assert_eq!(
        agree(&db, by_id, &[Value::Integer(4)]),
        vec![vec![
            Value::Integer(4),
            Value::Varchar("n4".into()),
            Value::BigInt(40),
            Value::Double(1.0)
        ]]
    );
    assert!(agree(&db, by_id, &[Value::Integer(10)]).is_empty(), "10 % 7 is 3");
    // Values that need the plan's cast or comparison, and the null that matches nothing.
    for value in [
        Value::SmallInt(5),
        Value::Varchar("6".into()),
        Value::Double(7.0),
        Value::BigInt(i64::MAX),
        Value::Null,
    ] {
        agree(&db, by_id, &[value]);
    }

    let by_name = "SELECT n AS q, t.id, d FROM t WHERE ? = name";
    for name in ["n0", "n4999", "n10", "nope", ""] {
        agree(&db, by_name, &[Value::Varchar(name.into())]);
    }
    agree(&db, "SELECT x.* FROM t AS x WHERE x.id = $1", &[Value::Integer(8)]);
    agree(
        &db,
        "SELECT name FROM t WHERE id = ? AND id = ?",
        &[Value::Integer(8), Value::Integer(8)],
    );
    agree(&db, "SELECT name FROM t WHERE n = ?", &[Value::BigInt(80)]);
    agree(&db, "SELECT t.name FROM t x WHERE x.id = ?", &[Value::Integer(8)]);
    agree(&db, "SELECT nope FROM t WHERE id = ?", &[Value::Integer(8)]);

    let named = db.prepare("SELECT name FROM t WHERE id = $id").expect("prepares");
    let found = named.execute_named(&[("ID", Value::Integer(9))]).expect("runs");
    assert_eq!(rows(&found), vec![vec![Value::Varchar("n9".into())]]);
}

#[test]
fn a_key_of_two_columns_and_a_unique_index_find_their_rows() {
    let db = Database::new();
    db.execute("CREATE TABLE o (w INTEGER, d INTEGER, o BIGINT, v VARCHAR, PRIMARY KEY (w, d, o))")
        .expect("creates");
    db.execute("INSERT INTO o SELECT i % 3, i % 10, i, 'v' || i FROM range(3000) r(i)")
        .expect("loads");
    db.execute("CREATE UNIQUE INDEX ov ON o (v)").expect("indexes");
    let select = "SELECT v FROM o WHERE d = ? AND o = ? AND w = ?";
    for (w, d, o) in [(0, 0, 0), (1, 1, 1), (2, 9, 2999), (1, 9, 2999), (0, 0, 3000)] {
        agree(&db, select, &[Value::Integer(d), Value::BigInt(o), Value::Integer(w)]);
    }
    agree(&db, select, &[Value::Integer(0), Value::Integer(30), Value::Integer(0)]);
    agree(&db, "SELECT * FROM o WHERE v = ?", &[Value::Varchar("v17".into())]);
    // A part of the key is no key.
    agree(&db, "SELECT v FROM o WHERE w = ? AND d = ?", &[Value::Integer(0), Value::Integer(0)]);
}

#[test]
fn a_lookup_follows_every_change_to_the_rows() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR)").expect("creates");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?)").expect("prepares");
    let select = "SELECT v FROM t WHERE id = ?";
    for id in 0..300_i64 {
        insert.execute(&[Value::BigInt(id), Value::Varchar(format!("v{id}"))]).expect("inserts");
        agree(&db, select, &[Value::BigInt(id)]);
        agree(&db, select, &[Value::BigInt(id / 2)]);
    }
    db.execute("UPDATE t SET v = 'changed' WHERE id = 7").expect("updates");
    assert_eq!(
        agree(&db, select, &[Value::BigInt(7)]),
        vec![vec![Value::Varchar("changed".into())]]
    );
    db.execute("UPDATE t SET id = id + 1000 WHERE id < 10").expect("moves keys");
    assert!(agree(&db, select, &[Value::BigInt(7)]).is_empty());
    assert_eq!(agree(&db, select, &[Value::BigInt(1007)]).len(), 1);
    db.execute("DELETE FROM t WHERE id % 2 = 0").expect("deletes");
    for id in [1, 2, 11, 12, 299, 1001, 1002] {
        agree(&db, select, &[Value::BigInt(id)]);
    }
    db.execute("ALTER TABLE t ADD COLUMN extra INTEGER DEFAULT 5").expect("alters");
    agree(&db, "SELECT * FROM t WHERE id = ?", &[Value::BigInt(13)]);
    db.execute("DROP TABLE t").expect("drops");
    db.execute("CREATE TABLE t (v VARCHAR, id BIGINT PRIMARY KEY)").expect("creates again");
    db.execute("INSERT INTO t VALUES ('again', 13)").expect("inserts");
    agree(&db, select, &[Value::BigInt(13)]);

    db.execute("CREATE SCHEMA s").expect("creates the schema");
    db.execute("CREATE TABLE s.t (id BIGINT PRIMARY KEY, v VARCHAR)").expect("creates");
    db.execute("INSERT INTO s.t VALUES (13, 'in s')").expect("inserts");
    let prepared = db.prepare(select).expect("prepares");
    assert_eq!(
        rows(&prepared.execute(&[Value::BigInt(13)]).expect("runs"))[0][0],
        Value::Varchar("again".into())
    );
    db.execute("SET schema = 's'").expect("sets the schema");
    assert_eq!(
        rows(&prepared.execute(&[Value::BigInt(13)]).expect("runs"))[0][0],
        Value::Varchar("in s".into())
    );
}

#[test]
fn a_lookup_reads_a_file_and_the_rows_beside_it() {
    let path = path("file");
    let open = || Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let db = open();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR, n BIGINT)").expect("creates");
    db.execute("INSERT INTO t SELECT i::INTEGER, 'n' || i, i FROM range(20000) r(i)")
        .expect("loads");
    db.execute("CHECKPOINT").expect("checkpoints");
    let select = "SELECT name, n FROM t WHERE id = ?";
    for id in [0, 1, 9999, 19999, 20000] {
        agree(&db, select, &[Value::Integer(id)]);
    }
    db.execute("INSERT INTO t VALUES (20000, 'late', 1)").expect("inserts");
    for id in [0, 19999, 20000] {
        agree(&db, select, &[Value::Integer(id)]);
    }
    drop(db);

    let db = open();
    for id in [0, 12345, 20000, 20001] {
        agree(&db, select, &[Value::Integer(id)]);
    }
    db.execute("DELETE FROM t WHERE id BETWEEN 100 AND 200").expect("deletes");
    for id in [99, 100, 150, 201, 20000] {
        agree(&db, select, &[Value::Integer(id)]);
    }
    db.execute("CHECKPOINT").expect("checkpoints");
    db.execute("UPDATE t SET name = 'moved' WHERE id = 300").expect("updates");
    for id in [99, 150, 300, 20000] {
        agree(&db, select, &[Value::Integer(id)]);
    }
}

#[test]
fn a_lookup_inside_a_transaction_sees_what_the_transaction_does() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").expect("inserts");
    let conn = db.connect();
    let select = conn.prepare("SELECT v FROM t WHERE id = ?").expect("prepares");
    conn.execute("BEGIN").expect("begins");
    conn.execute("INSERT INTO t VALUES (3, 'c')").expect("inserts");
    conn.execute("DELETE FROM t WHERE id = 1").expect("deletes");
    let seen = |id| rows(&select.execute(&[Value::Integer(id)]).expect("runs"));
    assert_eq!(seen(3), vec![vec![Value::Varchar("c".into())]]);
    assert!(seen(1).is_empty());
    let outside = db.prepare("SELECT v FROM t WHERE id = ?").expect("prepares");
    let seen_outside = |id| rows(&outside.execute(&[Value::Integer(id)]).expect("runs"));
    assert!(seen_outside(3).is_empty());
    assert_eq!(seen_outside(1), vec![vec![Value::Varchar("a".into())]]);
    conn.execute("COMMIT").expect("commits");
    assert_eq!(seen_outside(3), vec![vec![Value::Varchar("c".into())]]);
    assert!(seen_outside(1).is_empty());
}

#[test]
fn a_lookup_finds_rows_appended_since_it_last_looked() {
    let path = path("append");
    let open = || Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let db = open();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR UNIQUE, n BIGINT)")
        .expect("creates");
    db.execute("CREATE TABLE o (w INTEGER, d INTEGER, v VARCHAR, PRIMARY KEY (w, d))")
        .expect("creates");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?, ?)").expect("prepares");
    let insert_o = db.prepare("INSERT INTO o VALUES (?, ?, ?)").expect("prepares");
    let by_id = "SELECT name, n FROM t WHERE id = ?";
    let by_name = "SELECT id FROM t WHERE name = ?";
    let by_wd = "SELECT v FROM o WHERE w = ? AND d = ?";
    let check = |ids: &[i64]| {
        for &id in ids {
            agree(&db, by_id, &[Value::BigInt(id)]);
            agree(&db, by_name, &[Value::Varchar(format!("n{id}"))]);
            agree(&db, by_wd, &[Value::Integer((id % 7) as i32), Value::Integer(id as i32)]);
        }
    };
    let add = |id: i64| {
        let name = if id % 11 == 0 { Value::Null } else { Value::Varchar(format!("n{id}")) };
        insert.execute(&[Value::BigInt(id), name, Value::BigInt(id * 2)]).expect("inserts");
        insert_o
            .execute(&[
                Value::Integer((id % 7) as i32),
                Value::Integer(id as i32),
                Value::Varchar(format!("o{id}")),
            ])
            .expect("inserts");
    };
    for id in 0..3000_i64 {
        add(id);
        if id % 97 == 0 {
            check(&[id, id - 1, id + 1, id / 2, 0]);
        }
    }
    check(&[0, 11, 2047, 2048, 2999, 3000]);
    // A key already there is refused the same way by the row and by the plan, and leaves the
    // table as it was.
    let again = insert.execute(&[Value::BigInt(5), Value::Varchar("x".into()), Value::BigInt(0)]);
    let planned = db.execute("INSERT INTO t VALUES (5, 'x', 0)");
    assert_eq!(again.expect_err("refused").to_string(), planned.expect_err("refused").to_string());
    let again = insert.execute(&[Value::BigInt(-5), Value::Varchar("n6".into()), Value::BigInt(0)]);
    let planned = db.execute("INSERT INTO t VALUES (-5, 'n6', 0)");
    assert_eq!(again.expect_err("refused").to_string(), planned.expect_err("refused").to_string());
    check(&[5, -5, 6]);
    // Many rows at once through the plan, then a file, rows beside the file, rows taken out of
    // the file, and rows after those.
    db.execute("INSERT INTO t SELECT i, 'n' || i, i FROM range(3000, 9000) r(i)").expect("loads");
    check(&[2999, 3000, 5000, 8999, 9000]);
    db.execute("CHECKPOINT").expect("checkpoints");
    check(&[0, 8999]);
    for id in 9000..9100 {
        add(id);
    }
    check(&[0, 8999, 9000, 9050, 9099, 9100]);
    db.execute("DELETE FROM t WHERE id BETWEEN 100 AND 200").expect("deletes");
    db.execute("DELETE FROM o WHERE d BETWEEN 100 AND 200").expect("deletes");
    check(&[99, 150, 201, 9099]);
    db.execute("CHECKPOINT").expect("checkpoints");
    db.execute("DELETE FROM t WHERE id % 13 = 0").expect("deletes");
    check(&[13, 14, 9099]);
    for id in 9100..9200 {
        add(id);
        if id % 9 == 0 {
            check(&[id, 26, 27, 150, 9099]);
        }
    }
    add(150);
    check(&[150, 9199, 9200]);
    drop(db);
    let db = open();
    let select = db.prepare(by_id).expect("prepares");
    let found = select.execute(&[Value::BigInt(9199)]).expect("runs");
    assert_eq!(rows(&found), vec![vec![Value::Varchar("n9199".into()), Value::BigInt(18398)]]);
}
