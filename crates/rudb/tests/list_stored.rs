//! A list column in a file, through a checkpoint and a reopen.
//!
//! A list column was refused at the first checkpoint, with "native storage for INTEGER[]", so a
//! table with an array column lived only as long as its log. It is now written as one byte string a
//! row, and every query below must give over the file what it gives over the same rows in memory.

use rudb::Database;
use rudb_common::Value;

/// Twenty thousand rows. One row in eleven is null, one in seven is an empty list, and the rest
/// hold up to four integers, with a null element in some of them, and a list of text.
const ROWS: &str = "SELECT r::BIGINT AS k, \
                    CASE WHEN r % 11 = 0 THEN NULL \
                    WHEN r % 7 = 0 THEN []::INTEGER[] \
                    WHEN r % 5 = 0 THEN [r::INTEGER, NULL] \
                    ELSE [r::INTEGER, (r % 3)::INTEGER, 4, -r::INTEGER][1:(1 + r % 4)::BIGINT] END AS l, \
                    CASE WHEN r % 13 = 0 THEN NULL ELSE ['a' || (r % 9)::VARCHAR, ''] END AS s, \
                    [[r::SMALLINT], []::SMALLINT[]] AS n \
                    FROM range(20000) AS t(r)";

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

const QUERIES: &[&str] = &[
    "SELECT count(*), count(l), count(s), sum(len(l)), sum(len(n)) FROM t",
    "SELECT k, l, s, n FROM t WHERE k % 997 = 0 OR k < 30 ORDER BY k",
    "SELECT k FROM t WHERE list_contains(l, 4) AND k < 100 ORDER BY k",
    "SELECT l[1], count(*) FROM t WHERE k < 200 GROUP BY 1 ORDER BY 1 NULLS FIRST",
    "SELECT s, count(*) FROM t GROUP BY s ORDER BY s NULLS FIRST",
];

#[test]
fn a_list_column_survives_a_checkpoint_and_a_reopen() {
    let path = std::env::temp_dir().join(format!("rudb-list-stored-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");

    let memory = Database::new();
    memory.execute(&format!("CREATE TABLE t AS {ROWS}")).expect("loads in memory");
    let expected = QUERIES.iter().map(|sql| rows(&memory, sql)).collect::<Vec<_>>();
    assert!(expected[1].len() > 30, "the sample finds its rows");

    {
        let database = Database::open(name).expect("a file name starts a native database");
        database.execute(&format!("CREATE TABLE t AS {ROWS}")).expect("loads");
        database.execute("CHECKPOINT").expect("commits");
        for (sql, expected) in QUERIES.iter().zip(&expected) {
            assert_eq!(&rows(&database, sql), expected, "{sql}");
        }
        database.execute("INSERT INTO t VALUES (-1, [1, 2], NULL, [[]])").expect("one more row");
        database.execute("CHECKPOINT").expect("commits again");
    }

    let reopened = Database::open(name).expect("reopens");
    memory.execute("INSERT INTO t VALUES (-1, [1, 2], NULL, [[]])").expect("one more row");
    for sql in QUERIES {
        assert_eq!(rows(&reopened, sql), rows(&memory, sql), "{sql} after a reopen");
    }
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}
