//! An enum column in a file, through a checkpoint and a reopen.
//!
//! An enum column was refused at the first checkpoint, with "native storage for ENUM(...)", so a
//! table with one lived only as long as its log. Its labels now go into the directory with its type
//! and its pages hold the positions, and every query below must give over the file what it gives
//! over the same rows in memory.

use rudb::Database;
use rudb_common::Value;

/// Sixty thousand rows, with enums of three, three hundred and seventy thousand labels, so that the
/// positions are held in one, two and four bytes, and a list of one. One row in eleven has no mood.
const ROWS: &str = "SELECT r::BIGINT AS k, \
                    CASE WHEN r % 11 = 0 THEN NULL ELSE ['sad', 'ok', 'happy'][1 + r % 3] END\
                    ::ENUM('sad', 'ok', 'happy') AS m, \
                    medium_enum AS e, large_enum AS g, [medium_enum, NULL] AS l \
                    FROM test_all_types(use_large_enum := true), range(20000) AS t(r)";

/// One more row, put in after the first checkpoint.
const INSERT: &str = "INSERT INTO t VALUES (-1, 'ok', 'enum_7', NULL, ['enum_8'])";

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

const QUERIES: &[&str] = &[
    "SELECT count(*), count(m), count(g), count(DISTINCT m), min(e), count(DISTINCT g) FROM t",
    "SELECT k, m, e, g, l FROM t WHERE k % 997 = 0 OR k < 10 ORDER BY k, e NULLS FIRST",
    "SELECT m, count(*) FROM t GROUP BY m ORDER BY m NULLS FIRST",
    "SELECT count(*) FROM t WHERE m = 'happy' AND g = 'enum_69999'",
    "SELECT typeof(m), typeof(e) = typeof(g), typeof(l) = typeof(e) || '[]' FROM t LIMIT 1",
];

#[test]
fn an_enum_column_survives_a_checkpoint_and_a_reopen() {
    let path = std::env::temp_dir().join(format!("rudb-enum-stored-{}.rudb", std::process::id()));
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
        database.execute(INSERT).expect("one more row");
        database.execute("CHECKPOINT").expect("commits again");
    }

    let reopened = Database::open(name).expect("reopens");
    memory.execute(INSERT).expect("one more row");
    for sql in QUERIES {
        assert_eq!(rows(&reopened, sql), rows(&memory, sql), "{sql} after a reopen");
    }
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}
