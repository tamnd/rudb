//! A prepared upsert of one row looks its key up and either puts the row in or writes the held row
//! where it is, rather than reading the whole table through the plan, and has to leave the table
//! exactly as the plan would. Each check here runs the same values through the statement on two
//! databases, one where the table is plain and the short way takes it, and one where the table has
//! a `CHECK (true)`, which sends the statement through the plan and refuses nothing, and compares
//! the answers, the errors and the tables.

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

    fn explain(&self, sql: &str) -> String {
        let planned = self.planned.prepare(sql).expect("prepares").explain();
        assert_eq!(planned, "PIPELINE", "{sql}");
        self.short.prepare(sql).expect("prepares").explain()
    }

    /// Runs `sql` with `values` on both and checks they agree, returning the count it answered.
    fn run(&self, sql: &str, values: &[Value]) -> Option<Value> {
        let short = self.short.prepare(sql).expect("prepares").execute(values);
        let planned = self.planned.prepare(sql).expect("prepares").execute(values);
        let answer = match (short, planned) {
            (Ok(short), Ok(planned)) => {
                assert_eq!(short.value_at(0, 0), planned.value_at(0, 0), "{sql} {values:?}");
                Some(short.value_at(0, 0))
            }
            (Err(short), Err(planned)) => {
                assert_eq!(short.to_string(), planned.to_string(), "{sql} {values:?}");
                None
            }
            (short, planned) => {
                panic!("{sql} {values:?}: the short way said {short:?} and the plan {planned:?}")
            }
        };
        self.same();
        answer
    }

    fn same(&self) {
        let all = "SELECT * FROM t ORDER BY ALL";
        assert_eq!(rows(&self.short, all), rows(&self.planned, all));
    }
}

const COLUMNS: &str = "id BIGINT PRIMARY KEY, name VARCHAR, n BIGINT, d DOUBLE";

fn row(id: i64, name: Option<&str>, n: Option<i64>, d: Option<f64>) -> Vec<Value> {
    vec![
        Value::BigInt(id),
        name.map_or(Value::Null, |name| Value::Varchar(name.into())),
        n.map_or(Value::Null, Value::BigInt),
        d.map_or(Value::Null, Value::Double),
    ]
}

fn seeded() -> Pair {
    let pair = Pair::memory(COLUMNS);
    for db in [&pair.short, &pair.planned] {
        db.execute("INSERT INTO t SELECT i, 'v' || i, i, i / 2 FROM range(3000) r(i)")
            .expect("inserts");
    }
    pair
}

#[test]
fn each_action_lands_as_the_plan_lands_it() {
    let pair = seeded();
    let one = Some(Value::BigInt(1));
    let none = Some(Value::BigInt(0));
    let nothing = "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING";
    let ignore = "INSERT OR IGNORE INTO t VALUES (?, ?, ?, ?)";
    let replace = "INSERT OR REPLACE INTO t VALUES (?, ?, ?, ?)";
    for sql in [nothing, ignore, replace] {
        assert_eq!(pair.explain(sql), "Upsert t(id)", "{sql}");
    }
    for (pass, (sql, held)) in
        [(nothing, &none), (ignore, &none), (replace, &one)].iter().enumerate()
    {
        for id in [0, 7, 2047, 2048, 2999] {
            assert_eq!(pair.run(sql, &row(id, Some("x"), Some(-1), None)), **held, "{sql}");
        }
        // Keys no pass before this one put in.
        let pass = 100 * pass as i64;
        for id in [3000 + pass, 3001 + pass, -5 - pass] {
            assert_eq!(pair.run(sql, &row(id, None, Some(id), Some(0.5))), one, "{sql}");
        }
    }
    // A value the short way would have to cast, and a null for the key, go the long way.
    let cast = [Value::Varchar("8".into()), Value::Integer(1), Value::Integer(2), Value::Null];
    assert_eq!(pair.run(replace, &cast), one);
    assert_eq!(pair.run(replace, &row(9, None, None, None)), one);
    assert_eq!(pair.run(replace, &[Value::Null, Value::Null, Value::Null, Value::Null]), None);
}

#[test]
fn a_do_update_sets_what_it_says() {
    let pair = seeded();
    let one = Some(Value::BigInt(1));
    let add = "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT (id) DO UPDATE SET \
               name = excluded.name, n = n + excluded.n";
    assert_eq!(pair.explain(add), "Upsert t(id)");
    for id in [1, 1, 2, 2999, 3000, 3000, -1] {
        assert_eq!(pair.run(add, &row(id, Some("u"), Some(10), Some(1.0))), one);
    }
    // Nulls in, an overflow, and a value that would need a cast.
    assert_eq!(pair.run(add, &row(3, None, None, None)), one);
    assert_eq!(pair.run(add, &row(4, Some("big"), Some(i64::MAX), None)), None);
    let cast = [Value::BigInt(5), Value::Integer(1), Value::Integer(2), Value::Null];
    assert_eq!(pair.run(add, &cast), one);

    let shapes = [
        "INSERT INTO t (id, n) VALUES (?, ?) ON CONFLICT DO UPDATE SET n = t.n - excluded.n, \
         name = NULL",
        "INSERT INTO t (id, n) VALUES (?, ?) ON CONFLICT DO UPDATE SET d = excluded.n, \
         name = excluded.name",
        "INSERT INTO t AS a (n, id) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET n = a.n + $3",
        "INSERT INTO t (id, n) VALUES ($1, $2) ON CONFLICT DO UPDATE SET n = $3 + n, d = $4",
    ];
    for (at, sql) in shapes.iter().enumerate() {
        assert_eq!(pair.explain(sql), "Upsert t(id)", "{sql}");
        let width = pair.short.prepare(sql).expect("prepares").parameters().len();
        for id in [20, 21, 3500 + at as i64] {
            let values: Vec<Value> = [Value::BigInt(id), Value::BigInt(7), Value::BigInt(5)]
                .into_iter()
                .chain([Value::Double(2.5)])
                .take(width)
                .collect();
            let values = if at == 2 {
                [vec![values[1].clone(), values[0].clone()], values[2..].to_vec()].concat()
            } else {
                values
            };
            assert_eq!(pair.run(sql, &values), one, "{sql}");
        }
    }

    // What goes the long way, and has to come out the same all the same.
    let long = [
        "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT DO UPDATE SET id = excluded.id",
        "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT DO UPDATE SET n = 1",
        "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT DO UPDATE SET n = excluded.n \
         WHERE t.n > 100",
        "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT DO UPDATE SET n = n * excluded.n",
        "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT (name) DO NOTHING",
    ];
    for sql in long {
        let short = pair.short.prepare(sql).map(|prepared| prepared.explain());
        if let Ok(plan) = short {
            assert_eq!(plan, "PIPELINE", "{sql}");
        }
        for id in [30, 3600] {
            let values = row(id, Some("w"), Some(3), Some(1.0));
            match (pair.short.prepare(sql), pair.planned.prepare(sql)) {
                (Ok(_), Ok(_)) => {
                    pair.run(sql, &values);
                }
                (Err(short), Err(planned)) => assert_eq!(short.to_string(), planned.to_string()),
                (short, planned) => panic!("{sql}: {short:?} and {planned:?}"),
            }
        }
    }

    // The keys are found where the rows are after all of that.
    for db in [&pair.short, &pair.planned] {
        let lookup = db.prepare("SELECT name, n FROM t WHERE id = ?").expect("prepares");
        let found = lookup.execute(&[Value::BigInt(1)]).expect("reads");
        assert_eq!(
            found.rows().collect::<Vec<_>>(),
            vec![vec![Value::Varchar("u".into()), Value::BigInt(21)]]
        );
    }
}

#[test]
fn a_table_with_two_keys_or_a_text_key_is_found_the_right_way() {
    let pair = Pair::memory("id BIGINT PRIMARY KEY, code VARCHAR UNIQUE, n BIGINT");
    let sql = "INSERT INTO t VALUES (?, ?, ?) ON CONFLICT (id) DO UPDATE SET n = n + excluded.n";
    assert_eq!(pair.explain(sql), "PIPELINE");
    let values =
        |id: i64, code: &str| [Value::BigInt(id), Value::Varchar(code.into()), Value::BigInt(1)];
    pair.run(sql, &values(1, "a"));
    pair.run(sql, &values(1, "a"));
    pair.run(sql, &values(2, "a"));

    let pair = Pair::memory("ycsb_key VARCHAR PRIMARY KEY, field0 VARCHAR, hits INTEGER");
    let sql = "INSERT INTO t VALUES (?, ?, ?) ON CONFLICT DO UPDATE SET \
               field0 = excluded.field0, hits = hits + excluded.hits";
    assert_eq!(pair.explain(sql), "Upsert t(ycsb_key)");
    for round in 0..300 {
        let key = format!("user{}", round % 70);
        let values = [Value::Varchar(key), Value::Varchar(format!("f{round}")), Value::Integer(1)];
        assert_eq!(pair.run(sql, &values), Some(Value::BigInt(1)));
    }
    assert_eq!(
        rows(&pair.short, "SELECT count(*), sum(hits)::BIGINT FROM t"),
        vec![vec![Value::BigInt(70), Value::BigInt(300)]]
    );
}

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-upsert-point-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

#[test]
fn upserts_survive_a_crash() {
    let (short, planned) = (path("short"), path("planned"));
    let open =
        |path: &PathBuf| Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let pair = Pair::new(open(&short), open(&planned), COLUMNS);
    let sql = "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT DO UPDATE SET \
               name = excluded.name, n = n + excluded.n";
    for round in 0..2000_i64 {
        let id = round % 300;
        assert_eq!(
            pair.run(sql, &row(id, Some(&format!("r{round}")), Some(1), Some(0.5))),
            Some(Value::BigInt(1))
        );
        if round == 900 {
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
        rows(&pair.short, "SELECT count(*), sum(n)::BIGINT, max(name) FROM t WHERE id = 7"),
        vec![vec![Value::BigInt(1), Value::BigInt(7), Value::Varchar("r1807".into())]]
    );
    drop(pair);
    for path in [short, planned] {
        let _ = std::fs::remove_file(&path);
    }
}
