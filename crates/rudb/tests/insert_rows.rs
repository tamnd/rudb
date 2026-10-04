//! A prepared `INSERT ... VALUES` of several rows of parameters goes straight into the table as a
//! one row insert does, and has to land exactly what the plan would, all of the rows or none. Each
//! check here runs the same values through the statement on two databases, one where the table is
//! plain and the short way takes it, and one where the table has a `CHECK (true)`, which sends the
//! statement through the plan and refuses nothing, and compares the answers, the errors and the
//! tables.

use std::path::PathBuf;

use rudb::Database;
use rudb_common::Value;

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.execute(sql).expect(sql).rows().collect()
}

struct Pair {
    short: Database,
    planned: Database,
}

impl Pair {
    fn new(short: Database, planned: Database, columns: &str) -> Self {
        short.execute(&format!("CREATE TABLE t ({columns})")).expect("creates");
        planned.execute(&format!("CREATE TABLE t ({columns}, CHECK (true))")).expect("creates");
        Self { short, planned }
    }

    fn memory(columns: &str) -> Self {
        Self::new(Database::new(), Database::new(), columns)
    }

    /// Runs `insert` with `values` on both and checks they agree, returning whether it worked.
    fn insert(&self, insert: &str, values: &[Value]) -> bool {
        let short = self.short.prepare(insert).expect("prepares");
        let planned = self.planned.prepare(insert).expect("prepares");
        assert_eq!(planned.explain(), "PIPELINE", "{insert}");
        let worked = match (short.execute(values), planned.execute(values)) {
            (Ok(short), Ok(planned)) => {
                assert_eq!(short.value_at(0, 0), planned.value_at(0, 0), "{insert} {values:?}");
                true
            }
            (Err(short), Err(planned)) => {
                assert_eq!(short.to_string(), planned.to_string(), "{insert} {values:?}");
                false
            }
            (short, planned) => {
                panic!("{insert} {values:?}: the short way said {short:?} and the plan {planned:?}")
            }
        };
        // A failed insert can leave a transaction aborted, which refuses the read.
        if worked {
            self.same();
        }
        worked
    }

    fn same(&self) {
        let all = "SELECT * FROM t ORDER BY ALL";
        assert_eq!(rows(&self.short, all), rows(&self.planned, all));
    }
}

fn marks(rows: usize, width: usize) -> String {
    let row = format!("({})", vec!["?"; width].join(", "));
    vec![row; rows].join(", ")
}

#[test]
fn rows_of_every_kind_land_as_the_plan_lands_them() {
    let pair = Pair::memory("id BIGINT, name VARCHAR, price DOUBLE, qty INTEGER");
    let three = format!("INSERT INTO t VALUES {}", marks(3, 4));
    assert_eq!(pair.short.prepare(&three).expect("prepares").explain(), "InsertRows t");
    let one = format!("INSERT INTO t VALUES {}", marks(1, 4));
    assert_eq!(pair.short.prepare(&one).expect("prepares").explain(), "InsertOne t");
    let values = [
        [Value::BigInt(1), Value::Varchar("one".into()), Value::Double(1.5), Value::Integer(2)],
        [Value::Integer(2), Value::Null, Value::Integer(3), Value::BigInt(4)],
        [Value::SmallInt(3), Value::Varchar("x".into()), Value::Float(0.25), Value::TinyInt(5)],
    ]
    .concat();
    assert!(pair.insert(&three, &values));
    // A value the short way would have to cast sends the whole statement the long way.
    let mut cast = values.clone();
    cast[7] = Value::Varchar("9".into());
    assert!(pair.insert(&three, &cast));
    let mut wrong = values.clone();
    wrong[11] = Value::Varchar("seven".into());
    assert!(!pair.insert(&three, &wrong));
    // Nulls written into the statement, a column list, and parameters by number.
    let insert = "INSERT INTO t (qty, id) VALUES ($1, NULL), (NULL, $2), ($3, $4)";
    assert!(pair.insert(
        insert,
        &[Value::Integer(7), Value::BigInt(8), Value::Integer(9), Value::Integer(10)]
    ));
    let insert = "INSERT INTO t (name, id) VALUES (?, ?), (?, ?)";
    let named = |id: i64| [Value::Varchar(format!("n{id}")), Value::BigInt(id)];
    assert!(pair.insert(insert, &[named(10), named(11)].concat()));
    let got = rows(&pair.short, "SELECT count(*), sum(id), max(qty) FROM t");
    assert_eq!(got, rows(&pair.planned, "SELECT count(*), sum(id), max(qty) FROM t"));
}

#[test]
fn a_row_refused_leaves_out_every_row_of_its_statement() {
    let pair = Pair::memory("id INTEGER PRIMARY KEY, qty INTEGER NOT NULL, note VARCHAR UNIQUE");
    let insert = format!("INSERT INTO t VALUES {}", marks(3, 3));
    let row = |id: i32, qty: Option<i32>, note: &str| {
        [Value::Integer(id), qty.map_or(Value::Null, Value::Integer), Value::Varchar(note.into())]
    };
    assert!(pair.insert(
        &insert,
        &[row(1, Some(1), "a"), row(2, Some(2), "b"), row(3, Some(3), "c")].concat()
    ));
    // A key the batch repeats, one already there in the last row, a null in the middle row, and
    // a unique value repeated.
    for values in [
        [row(4, Some(4), "d"), row(5, Some(5), "e"), row(4, Some(6), "f")],
        [row(4, Some(4), "d"), row(5, Some(5), "e"), row(1, Some(6), "f")],
        [row(4, Some(4), "d"), row(5, None, "e"), row(6, Some(6), "f")],
        [row(4, Some(4), "d"), row(5, Some(5), "a"), row(6, Some(6), "f")],
        [row(4, Some(4), "d"), row(5, Some(5), "e"), row(6, Some(6), "e")],
    ] {
        assert!(!pair.insert(&insert, &values.concat()));
    }
    assert!(pair.insert(
        &insert,
        &[row(4, Some(4), "d"), row(5, Some(5), "e"), row(6, Some(6), "f")].concat()
    ));
    // And the keys are found where the rows went.
    for db in [&pair.short, &pair.planned] {
        let lookup = db.prepare("SELECT note FROM t WHERE id = ?").expect("prepares");
        let found = lookup.execute(&[Value::Integer(5)]).expect("reads");
        assert_eq!(found.rows().collect::<Vec<_>>(), vec![vec![Value::Varchar("e".into())]]);
    }
}

#[test]
fn rows_put_in_inside_a_transaction_go_with_it() {
    let pair = Pair::memory("id BIGINT PRIMARY KEY, v VARCHAR");
    let insert = format!("INSERT INTO t VALUES {}", marks(4, 2));
    let batch = |from: i64| {
        (from..from + 4)
            .flat_map(|id| [Value::BigInt(id), Value::Varchar(format!("v{id}"))])
            .collect::<Vec<_>>()
    };
    let both = |sql: &str| {
        let short = pair.short.execute(sql).map_err(|error| error.to_string());
        let planned = pair.planned.execute(sql).map_err(|error| error.to_string());
        assert_eq!(short.is_ok(), planned.is_ok(), "{sql}");
    };
    both("BEGIN");
    assert_eq!(pair.short.prepare(&insert).expect("prepares").explain(), "InsertRows t");
    for round in 0..5 {
        assert!(pair.insert(&insert, &batch(round * 4)));
    }
    both("COMMIT");
    // Rolled back, and then a key repeated, which aborts the transaction and the next insert
    // with it.
    both("BEGIN");
    assert!(pair.insert(&insert, &batch(100)));
    both("ROLLBACK");
    both("BEGIN");
    assert!(pair.insert(&insert, &batch(200)));
    assert!(!pair.insert(&insert, &batch(202)));
    assert!(!pair.insert(&insert, &batch(300)));
    both("COMMIT");
    both("ROLLBACK");
    pair.same();
    assert_eq!(rows(&pair.short, "SELECT count(*) FROM t"), vec![vec![Value::BigInt(20)]]);
    assert!(!pair.insert(&insert, &batch(16)));
    pair.same();
    assert!(pair.insert(&insert, &batch(20)));
}

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-insert-rows-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

#[test]
fn rows_put_in_together_survive_a_crash() {
    let (short, planned) = (path("short"), path("planned"));
    let open =
        |path: &PathBuf| Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let pair =
        Pair::new(open(&short), open(&planned), "id BIGINT PRIMARY KEY, v VARCHAR, n DOUBLE");
    let insert = format!("INSERT INTO t VALUES {}", marks(10, 3));
    for round in 0..50_i64 {
        let values: Vec<Value> = (round * 10..round * 10 + 10)
            .flat_map(|id| [Value::BigInt(id), Value::Varchar(format!("v{id}")), Value::Integer(1)])
            .collect();
        assert!(pair.insert(&insert, &values));
        if round == 20 {
            pair.short.execute("CHECKPOINT").expect("checkpoints");
            pair.planned.execute("CHECKPOINT").expect("checkpoints");
        }
    }
    let Pair { short: a, planned: b } = pair;
    std::mem::forget(a);
    std::mem::forget(b);
    let pair = Pair { short: open(&short), planned: open(&planned) };
    pair.same();
    assert_eq!(
        rows(&pair.short, "SELECT count(*), sum(n), max(v) FROM t"),
        vec![vec![Value::BigInt(500), Value::Double(500.0), Value::Varchar("v99".into())]]
    );
    let values =
        [Value::BigInt(7), Value::Null, Value::Null, Value::BigInt(1000), Value::Null, Value::Null];
    assert!(!pair.insert(&format!("INSERT INTO t VALUES {}", marks(2, 3)), &values));
    drop(pair);
    for path in [short, planned] {
        let _ = std::fs::remove_file(&path);
    }
}
