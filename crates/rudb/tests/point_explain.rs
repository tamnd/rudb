//! `Prepared::explain` names the point plan a prepared statement runs as, which is what the YCSB
//! driver checks before it measures, and says `PIPELINE` for anything that would go through the
//! plan. Each name here is checked against the statement running: a statement named a point plan
//! answers what the plan does, and one named `PIPELINE` is one the short ways leave alone.

use rudb::Database;
use rudb_common::Value;

const SCHEMA: &str = "CREATE TABLE usertable (ycsb_key VARCHAR PRIMARY KEY, field0 VARCHAR, \
                      field1 VARCHAR, field2 VARCHAR)";

fn explain(db: &Database, sql: &str) -> String {
    db.prepare(sql).expect(sql).explain()
}

#[test]
fn the_ycsb_statements_name_their_point_plans() {
    let db = Database::new();
    db.execute(SCHEMA).expect("creates");
    let insert = "INSERT INTO usertable (ycsb_key, field0, field1, field2) VALUES (?, ?, ?, ?)";
    assert_eq!(explain(&db, insert), "InsertOne usertable");
    let read = "SELECT * FROM usertable WHERE ycsb_key = ?";
    assert_eq!(explain(&db, read), "POINT Lookup usertable(ycsb_key)");
    for field in ["field0", "field1", "field2"] {
        let update = format!("UPDATE usertable SET {field} = ? WHERE ycsb_key = ?");
        assert_eq!(explain(&db, &update), format!("UpdateOne usertable(ycsb_key) SET {field}"));
    }
    db.execute("CREATE TABLE counts (id BIGINT PRIMARY KEY, n BIGINT, m BIGINT)").expect("creates");
    let delta = "UPDATE counts SET n = n + ?, m = m - ? WHERE id = ?";
    assert_eq!(explain(&db, delta), "DeltaOne counts(id) SET n, m");
    let mixed = "UPDATE counts SET n = n + ?, m = ? WHERE id = ?";
    assert_eq!(explain(&db, mixed), "UpdateOne counts(id) SET n, m");
    let scan = "SELECT * FROM usertable WHERE ycsb_key >= ? ORDER BY ycsb_key LIMIT ?";
    assert_eq!(explain(&db, scan), "Range usertable(ycsb_key)");

    // And they run: the rows go in, are read, written and scanned.
    let insert = db.prepare(insert).expect("prepares");
    for at in 0..20 {
        let key = Value::Varchar(format!("user{at:03}"));
        let field = Value::Varchar(format!("v{at}"));
        insert.execute(&[key, field.clone(), field.clone(), field]).expect("inserts");
    }
    let read = db.prepare(read).expect("prepares");
    let found = read.execute(&[Value::Varchar("user007".into())]).expect("reads");
    assert_eq!(found.value_at(0, 1), Value::Varchar("v7".into()));
    let update =
        db.prepare("UPDATE usertable SET field1 = ? WHERE ycsb_key = ?").expect("prepares");
    update
        .execute(&[Value::Varchar("u".into()), Value::Varchar("user007".into())])
        .expect("updates");
    let scan = db.prepare(scan).expect("prepares");
    let rows = scan.execute(&[Value::Varchar("user006".into()), Value::BigInt(3)]).expect("scans");
    let keys: Vec<Value> = rows.rows().map(|row| row[0].clone()).collect();
    assert_eq!(keys, ["user006", "user007", "user008"].map(|key| Value::Varchar(key.into())));
    assert_eq!(rows.value_at(1, 2), Value::Varchar("u".into()));
}

#[test]
fn what_goes_through_the_plan_is_named_a_pipeline() {
    let db = Database::new();
    db.execute(SCHEMA).expect("creates");
    db.execute("CREATE TABLE checked (id BIGINT PRIMARY KEY, n BIGINT CHECK (n >= 0))")
        .expect("creates");
    db.execute("CREATE TABLE floats (x DOUBLE PRIMARY KEY, n BIGINT)").expect("creates");
    for sql in [
        // Not by the key.
        "SELECT * FROM usertable WHERE field0 = ?",
        "SELECT * FROM usertable WHERE field0 >= ? ORDER BY field0 LIMIT 10",
        "UPDATE usertable SET field1 = ? WHERE field0 = ?",
        // Writes the key, or into a table with a check.
        "UPDATE usertable SET ycsb_key = ? WHERE ycsb_key = ?",
        "UPDATE checked SET n = ? WHERE id = ?",
        // A range of a key the short way has no order for, and one too long.
        "SELECT * FROM floats WHERE x >= ? ORDER BY x LIMIT 10",
        "SELECT * FROM usertable WHERE ycsb_key >= ? ORDER BY ycsb_key LIMIT 5000",
        // No such table yet, and a statement of no point shape at all.
        "SELECT * FROM later WHERE id = ?",
        "SELECT count(*) FROM usertable WHERE ycsb_key > ?",
    ] {
        assert_eq!(explain(&db, sql), "PIPELINE", "{sql}");
    }
    // A name follows the catalog: the table the statement was prepared before now exists.
    let later = db.prepare("SELECT * FROM later WHERE id = ?").expect("prepares");
    db.execute("CREATE TABLE later (id INTEGER PRIMARY KEY, v VARCHAR)").expect("creates");
    assert_eq!(later.explain(), "POINT Lookup later(id)");
    // Inside a transaction everything but an insert goes through the plan, and an insert too once
    // the transaction is read only or aborted.
    let read = db.prepare("SELECT * FROM usertable WHERE ycsb_key = ?").expect("prepares");
    let insert = db.prepare("INSERT INTO usertable VALUES (?, ?, ?, ?)").expect("prepares");
    db.execute("BEGIN").expect("begins");
    assert_eq!(read.explain(), "PIPELINE");
    assert_eq!(insert.explain(), "InsertOne usertable");
    let row = |key: &str| [key, "a", "b", "c"].map(|text| Value::Varchar(text.into()));
    insert.execute(&row("k")).expect("inserts");
    insert.execute(&row("k")).expect_err("a duplicate");
    assert_eq!(insert.explain(), "PIPELINE");
    db.execute("ROLLBACK").expect("rolls back");
    db.execute("BEGIN TRANSACTION READ ONLY").expect("begins");
    assert_eq!(insert.explain(), "PIPELINE");
    db.execute("COMMIT").expect("commits");
    assert_eq!(read.explain(), "POINT Lookup usertable(ycsb_key)");
    assert_eq!(insert.explain(), "InsertOne usertable");
}
