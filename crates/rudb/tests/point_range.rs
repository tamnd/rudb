//! A prepared `SELECT ... WHERE key >= ? ORDER BY key LIMIT n` reads its rows in key order from
//! where the key is held rather than through the plan, and has to answer exactly what the plan
//! would. Each check here runs the same values through the statement, which can take the short
//! way, and through the same statement with `AND true` after the comparison, which cannot, and
//! compares the names, the types, the rows and the errors.

use std::path::PathBuf;

use rudb::Database;
use rudb_common::Value;

/// Runs `values` through the range read `select`, `where`, `order` and `limit`, and through the
/// plan, and checks they agree.
fn agree(db: &Database, select: &str, filter: &str, tail: &str, values: &[Value]) -> usize {
    let short = db.prepare(&format!("{select} WHERE {filter} {tail}")).expect("prepares");
    let planned =
        db.prepare(&format!("{select} WHERE {filter} AND true {tail}")).expect("prepares");
    match (short.execute(values), planned.execute(values)) {
        (Ok(short), Ok(planned)) => {
            let context = format!("{select} WHERE {filter} {tail} {values:?}");
            assert_eq!(short.names(), planned.names(), "{context}");
            assert_eq!(short.types(), planned.types(), "{context}");
            let rows = short.rows().collect::<Vec<_>>();
            assert_eq!(rows, planned.rows().collect::<Vec<_>>(), "{context}");
            rows.len()
        }
        (Err(short), Err(planned)) => {
            assert_eq!(short.to_string(), planned.to_string(), "{select} {filter} {values:?}");
            0
        }
        (short, planned) => {
            panic!(
                "{select} {filter} {values:?}: the short way said {short:?} and the plan {planned:?}"
            )
        }
    }
}

const FILTERS: &[&str] = &["id >= ?", "id > ?", "id <= ?", "id < ?", "? <= id", "? > id"];
const ORDERS: &[&str] = &["ORDER BY id", "ORDER BY id ASC", "ORDER BY id DESC", "ORDER BY t.id"];

fn sweep(db: &Database, bounds: &[i64]) -> usize {
    let mut found = 0;
    for filter in FILTERS {
        for order in ORDERS {
            for &bound in bounds {
                for limit in [0, 1, 10, 1000] {
                    let tail = format!("{order} LIMIT {limit}");
                    found += agree(db, "SELECT * FROM t", filter, &tail, &[Value::BigInt(bound)]);
                }
                let tail = format!("{order} LIMIT ?");
                let values = [Value::BigInt(bound), Value::BigInt(7)];
                found += agree(db, "SELECT name, id FROM t", filter, &tail, &values);
            }
        }
    }
    found
}

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-range-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

#[test]
fn a_short_range_by_key_reads_what_the_plan_does() {
    let path = path("sweep");
    let open = || Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let db = open();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR, n INTEGER)").expect("creates");
    // Keys out of order, with gaps.
    db.execute("INSERT INTO t SELECT (i * 7919) % 10007, 'n' || i, i FROM range(5000) r(i)")
        .expect("loads");
    let bounds = [-5, 0, 1, 2, 500, 5000, 10006, 10007, 20000];
    assert!(sweep(&db, &bounds) > 0);
    // Rows after every key there is, and then one before them.
    let insert = db.prepare("INSERT INTO t VALUES (?, ?, ?)").expect("prepares");
    let range = "SELECT id FROM t";
    for id in 20000..20050_i64 {
        insert
            .execute(&[Value::BigInt(id), Value::Varchar("late".into()), Value::Integer(1)])
            .expect("inserts");
        agree(&db, range, "id >= ?", "ORDER BY id LIMIT 3", &[Value::BigInt(id - 1)]);
        agree(&db, range, "id <= ?", "ORDER BY id DESC LIMIT 3", &[Value::BigInt(id + 1)]);
    }
    insert
        .execute(&[Value::BigInt(-7), Value::Varchar("early".into()), Value::Integer(1)])
        .expect("inserts");
    agree(&db, range, "id >= ?", "ORDER BY id LIMIT 3", &[Value::BigInt(-100)]);
    agree(&db, range, "id >= ?", "ORDER BY id LIMIT 3", &[Value::BigInt(20048)]);
    // Rows written where they are, rows taken out, a file and rows beside it.
    db.execute("UPDATE t SET name = 'moved' WHERE id % 3 = 0").expect("updates");
    sweep(&db, &[0, 5000, 20000]);
    db.execute("DELETE FROM t WHERE id % 5 = 0").expect("deletes");
    sweep(&db, &[0, 5000, 20000]);
    db.execute("CHECKPOINT").expect("checkpoints");
    sweep(&db, &[-7, 5000, 20049]);
    db.execute("DELETE FROM t WHERE id BETWEEN 100 AND 200").expect("deletes");
    sweep(&db, &[99, 150, 201]);
    drop(db);
    let db = open();
    sweep(&db, &[99, 150, 201, 20049]);
    // The order the session says when the statement says none.
    db.execute("SET default_order = 'DESC'").expect("sets");
    assert_eq!(agree(&db, range, "id >= ?", "ORDER BY id LIMIT 5", &[Value::BigInt(0)]), 5);
    db.execute("RESET default_order").expect("resets");
    // Values and limits the plan has to see to, and the names the select list gives.
    let tail = "ORDER BY id LIMIT ?";
    for values in [
        [Value::Integer(10), Value::Integer(3)],
        [Value::Null, Value::BigInt(3)],
        [Value::Varchar("10".into()), Value::BigInt(3)],
        [Value::Double(10.5), Value::BigInt(3)],
        [Value::BigInt(10), Value::BigInt(-1)],
        [Value::BigInt(10), Value::Null],
        [Value::BigInt(10), Value::BigInt(5000)],
        [Value::BigInt(i64::MIN), Value::BigInt(3)],
        [Value::BigInt(i64::MAX), Value::BigInt(3)],
    ] {
        agree(&db, "SELECT * FROM t", "id >= ?", tail, &values);
    }
    agree(&db, "SELECT n AS q, t.* FROM t", "id > ?", "ORDER BY id LIMIT 4", &[Value::BigInt(9)]);
    agree(
        &db,
        "SELECT x.name FROM t AS x",
        "x.id > ?",
        "ORDER BY x.id LIMIT 4",
        &[Value::BigInt(9)],
    );
    agree(
        &db,
        "SELECT name FROM t",
        "id > ?",
        "ORDER BY id NULLS FIRST LIMIT 4",
        &[Value::BigInt(9)],
    );
}

#[test]
fn a_short_range_reads_an_integer_key_and_a_unique_index() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, u BIGINT UNIQUE, name VARCHAR)")
        .expect("creates");
    db.execute(
        "INSERT INTO t SELECT i::INTEGER, CASE WHEN i % 4 = 0 THEN NULL ELSE 100 - i END, 'n' || i \
         FROM range(300) r(i)",
    )
    .expect("loads");
    for bound in [-1_i64, 0, 50, 299, 300, 1 << 40] {
        for filter in ["id >= ?", "id < ?"] {
            agree(
                &db,
                "SELECT name FROM t",
                filter,
                "ORDER BY id LIMIT 5",
                &[Value::BigInt(bound)],
            );
            agree(
                &db,
                "SELECT name FROM t",
                filter,
                "ORDER BY id DESC LIMIT 5",
                &[Value::Integer(bound as i32)],
            );
        }
        let filter = "u > ?";
        agree(
            &db,
            "SELECT id, u FROM t",
            filter,
            "ORDER BY u LIMIT 6",
            &[Value::BigInt(bound - 200)],
        );
        agree(
            &db,
            "SELECT id, u FROM t",
            filter,
            "ORDER BY u DESC LIMIT 6",
            &[Value::BigInt(bound - 200)],
        );
    }
    let conn = db.connect();
    conn.execute("BEGIN").expect("begins");
    conn.execute("INSERT INTO t VALUES (1000, 1000, 'mine')").expect("inserts");
    let read =
        conn.prepare("SELECT name FROM t WHERE id > ? ORDER BY id LIMIT 1").expect("prepares");
    let seen = read.execute(&[Value::BigInt(299)]).expect("runs");
    assert_eq!(seen.rows().collect::<Vec<_>>(), vec![vec![Value::Varchar("mine".into())]]);
    let outside =
        db.prepare("SELECT name FROM t WHERE id > ? ORDER BY id LIMIT 1").expect("prepares");
    assert_eq!(outside.execute(&[Value::BigInt(299)]).expect("runs").len(), 0);
    conn.execute("COMMIT").expect("commits");
    assert_eq!(outside.execute(&[Value::BigInt(299)]).expect("runs").len(), 1);
}

#[test]
fn a_short_range_reads_a_text_key_the_way_ycsb_scans() {
    let path = path("text");
    let open = || Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let db = open();
    db.execute(
        "CREATE TABLE usertable (ycsb_key VARCHAR PRIMARY KEY, field0 VARCHAR, field1 VARCHAR)",
    )
    .expect("creates");
    db.execute(
        "INSERT INTO usertable SELECT 'user' || ((i * 7919) % 10007), 'a' || i, 'b' || i \
         FROM range(3000) r(i)",
    )
    .expect("loads");
    let select = "SELECT * FROM usertable";
    let scan = |db: &Database, bounds: &[&str]| {
        let mut found = 0;
        for filter in ["ycsb_key >= ?", "ycsb_key > ?", "ycsb_key <= ?", "? > ycsb_key"] {
            for order in ["ORDER BY ycsb_key", "ORDER BY ycsb_key DESC"] {
                for bound in bounds {
                    let values = [Value::Varchar((*bound).into()), Value::BigInt(10)];
                    found += agree(db, select, filter, &format!("{order} LIMIT ?"), &values);
                    let values = [Value::Varchar((*bound).into())];
                    found += agree(db, select, filter, &format!("{order} LIMIT 100"), &values);
                }
            }
        }
        found
    };
    // Prefixes, a bound between two keys, the ends, bytes past ASCII and nothing at all.
    let bounds =
        ["", "user", "user1", "user5000", "user50001", "user9", "uses", "zz", "user\u{e9}"];
    assert!(scan(&db, &bounds) > 0);
    // A number for a text key goes the way of the plan, which casts it.
    agree(&db, select, "ycsb_key >= ?", "ORDER BY ycsb_key LIMIT 5", &[Value::BigInt(1)]);
    // A limit bound as text, the way a driver that binds everything as text sends it.
    for limit in
        ["0", "7", "1000", "1001", "", " 3", "+3", "3.0", "-1", "x", "99999999999999999999"]
    {
        let values = [Value::Varchar("user5".into()), Value::Varchar(limit.into())];
        agree(&db, select, "ycsb_key >= ?", "ORDER BY ycsb_key LIMIT ?", &values);
    }
    let scan_text = db
        .prepare("SELECT * FROM usertable WHERE ycsb_key >= ? ORDER BY ycsb_key LIMIT ?")
        .expect("prepares");
    let found = scan_text
        .execute(&[Value::Varchar("user5".into()), Value::Varchar("7".into())])
        .expect("scans");
    assert_eq!(found.len(), 7);
    // Keys after every key there is, and then one before them, as YCSB loads in order.
    let insert = db.prepare("INSERT INTO usertable VALUES (?, ?, ?)").expect("prepares");
    for at in 0..50 {
        let key = format!("user{}", 99_990 + at);
        insert
            .execute(&[Value::Varchar(key), Value::Null, Value::Varchar("x".into())])
            .expect("inserts");
        if at % 10 == 0 {
            assert!(scan(&db, &["user9998", "user99995"]) > 0);
        }
    }
    insert
        .execute(&[Value::Varchar("user00".into()), Value::Varchar("y".into()), Value::Null])
        .expect("inserts");
    assert!(scan(&db, &bounds) > 0);
    let update =
        db.prepare("UPDATE usertable SET field1 = ? WHERE ycsb_key = ?").expect("prepares");
    update.execute(&[Value::Varchar("u".into()), Value::Varchar("user1".into())]).expect("updates");
    db.execute("DELETE FROM usertable WHERE ycsb_key LIKE 'user2%'").expect("deletes");
    assert!(scan(&db, &bounds) > 0);
    db.execute("CHECKPOINT").expect("checkpoints");
    assert!(scan(&db, &bounds) > 0);
    drop((insert, update, db));
    let db = open();
    assert!(scan(&db, &bounds) > 0);
}
