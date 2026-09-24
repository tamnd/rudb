//! A scan leaves out of its narrowing the columns only its filter read, and the answers stay the
//! same.
//!
//! Each query here is asked twice, once as written and once with the filter's column also read
//! above the filter, which keeps the scan from leaving it out. The table is stored in a file so
//! that its columns come off the disk packed, which is the case the narrowing was unpacking.

use rudb::Database;
use rudb_common::Value;

/// A table in a file with an id, a join key, a quantity with some nulls and a day number, and a
/// small table to join it to.
struct Stored {
    database: Database,
    path: std::path::PathBuf,
}

impl Stored {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-unread-{name}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let name = path.to_str().expect("a UTF-8 temporary path");
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing
                .execute(
                    "CREATE TABLE t AS SELECT i AS id, i % 50 AS k, CASE WHEN i % 13 = 0 THEN \
                     NULL ELSE i % 7 END AS q, (i * 31) % 1000 AS d FROM range(100000) r(i)",
                )
                .expect("the table");
            writing
                .execute("CREATE TABLE u AS SELECT i AS k, i % 3 AS g FROM range(40) r(i)")
                .expect("the table to join to");
            writing.execute("CHECKPOINT").expect("the tables are committed");
        }
        let database = Database::open(name).expect("the written file opens again");
        Self { database, path }
    }

    fn rows(&self, sql: &str) -> Vec<Vec<Value>> {
        self.database.query(sql).expect("the query ran").rows().collect()
    }
}

impl Drop for Stored {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn a_column_only_the_filter_reads_changes_no_answer() {
    let stored = Stored::new("pairs");
    let pairs = [
        (
            "SELECT count(*), sum(q) FROM t WHERE d <= 980",
            "SELECT count(*), sum(q), max(d) >= 0 FROM t WHERE d <= 980",
        ),
        (
            "SELECT g, count(*), sum(t.q) FROM t JOIN u ON t.k = u.k WHERE t.d <= 980 GROUP BY g \
             ORDER BY g",
            "SELECT g, count(*), sum(t.q), max(t.d) >= 0 FROM t JOIN u ON t.k = u.k WHERE t.d <= \
             980 GROUP BY g ORDER BY g",
        ),
        (
            "SELECT id, q FROM t WHERE d <= 980 ORDER BY id DESC LIMIT 20",
            "SELECT id, q, d >= 0 FROM t WHERE d <= 980 ORDER BY id DESC LIMIT 20",
        ),
    ];
    for (plain, reading) in pairs {
        let plain_rows = stored.rows(plain);
        let reading_rows: Vec<Vec<Value>> = stored
            .rows(reading)
            .into_iter()
            .map(|mut row| {
                assert_eq!(row.pop(), Some(Value::Boolean(true)), "{reading}");
                row
            })
            .collect();
        assert!(!plain_rows.is_empty(), "{plain}");
        assert_eq!(plain_rows, reading_rows, "{plain}");
    }
}

/// A query whose answer is the scan's own rows hands the filter's column to the caller, so it has
/// to come out with its values rather than nulls.
#[test]
fn a_column_the_caller_gets_is_still_there() {
    let stored = Stored::new("caller");
    let rows = stored.rows("SELECT * FROM t WHERE d <= 980 ORDER BY id LIMIT 5");
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| matches!(row[3], Value::BigInt(d) if d <= 980)), "{rows:?}");
    let union = stored.rows(
        "SELECT d FROM t WHERE d <= 980 AND id < 10 UNION ALL SELECT d FROM t WHERE d > 990 AND id \
         < 100 ORDER BY 1",
    );
    assert!(union.iter().all(|row| !matches!(row[0], Value::Null)), "{union:?}");
}
