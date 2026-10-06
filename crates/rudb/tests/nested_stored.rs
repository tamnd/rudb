//! Struct, map, union and array columns in a file, through a checkpoint and a reopen.
//!
//! Each of these was refused at the first checkpoint, with "native storage for STRUCT(...)" and
//! the like, so a table with one lived only as long as its log. They are now written the way a
//! list is, one byte string a row, and every query below must give over the file what it gives
//! over the same rows in memory.

use rudb::Database;
use rudb_common::Value;

/// Twenty thousand rows. One row in eleven has a null struct, one in thirteen a null map, and the
/// union holds a number on even rows and text on odd ones, with a null member now and then.
const ROWS: &str = "SELECT r::BIGINT AS k, \
                    CASE WHEN r % 11 = 0 THEN NULL \
                    ELSE {'a': r::INTEGER, 'b': 'b' || (r % 9)::VARCHAR, 'c': [r, NULL]} END AS s, \
                    CASE WHEN r % 13 = 0 THEN NULL WHEN r % 7 = 0 THEN MAP {} \
                    ELSE MAP {r::INTEGER: 'v', -r::INTEGER: NULL} END::MAP(INTEGER, VARCHAR) AS m, \
                    CASE WHEN r % 17 = 0 THEN NULL \
                    WHEN r % 2 = 0 THEN r::UNION(n BIGINT, t VARCHAR) \
                    ELSE (r::VARCHAR)::UNION(n BIGINT, t VARCHAR) END AS u, \
                    [r::SMALLINT, NULL, 3]::SMALLINT[3] AS a, \
                    [{'x': r % 5, 'y': [r::DOUBLE]}] AS l, \
                    row(r::INTEGER, 'z') AS p \
                    FROM range(20000) AS t(r)";

/// One more row, put in after the first checkpoint.
const INSERT: &str = "INSERT INTO t VALUES (-1, {'a': 1, 'b': NULL, 'c': []}, MAP {1: 'one'}, \
                      'x'::UNION(n BIGINT, t VARCHAR), [NULL, NULL, NULL], [], row(NULL, NULL))";

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

const QUERIES: &[&str] = &[
    "SELECT count(*), count(s), count(m), count(u), count(a), count(l), count(p) FROM t",
    "SELECT * FROM t WHERE k % 997 = 0 OR k < 20 ORDER BY k",
    "SELECT s.b, count(*) FROM t GROUP BY 1 ORDER BY 1 NULLS FIRST",
    "SELECT sum(cardinality(m)), sum(u.n), count(u.t), sum(a[1]), sum(l[1].x) FROM t",
    "SELECT typeof(s), typeof(m), typeof(u), typeof(a), typeof(l), typeof(p) FROM t LIMIT 1",
];

#[test]
fn nested_columns_survive_a_checkpoint_and_a_reopen() {
    let path = std::env::temp_dir().join(format!("rudb-nested-stored-{}.rudb", std::process::id()));
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

#[test]
fn every_column_of_test_all_types_survives_a_reopen() {
    let path = std::env::temp_dir().join(format!("rudb-all-stored-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");

    let memory = Database::new();
    memory.execute("CREATE TABLE a AS SELECT * FROM test_all_types()").expect("loads");
    {
        let database = Database::open(name).expect("a file name starts a native database");
        database.execute("CREATE TABLE a AS SELECT * FROM test_all_types()").expect("loads");
        database.execute("CHECKPOINT").expect("commits");
    }
    let reopened = Database::open(name).expect("reopens");
    let columns = rows(&memory, "SELECT column_name FROM (DESCRIBE a)");
    let differ: Vec<&Value> = columns
        .iter()
        .map(|row| &row[0])
        .filter(|column| {
            let Value::Varchar(column) = column else { return true };
            let sql = format!("SELECT \"{column}\" FROM a");
            // As text, because a NaN in `double_array` is not equal to itself as a value.
            format!("{:?}", rows(&reopened, &sql)) != format!("{:?}", rows(&memory, &sql))
        })
        .collect();
    assert!(differ.is_empty(), "these columns differ after a reopen: {differ:?}");
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_json_document_inside_a_list_or_a_struct_survives_a_reopen() {
    let path = std::env::temp_dir().join(format!("rudb-json-nested-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");

    let sql = "SELECT k, j::VARCHAR, s.d::VARCHAR, b[1]::VARCHAR FROM t ORDER BY k";
    {
        let database = Database::open(name).expect("a file name starts a native database");
        for sql in [
            "CREATE TABLE t (k INTEGER, j JSON[], s STRUCT(d JSON), b JSONB[])",
            r#"INSERT INTO t VALUES (1, ['{"a": 1}', NULL], {'d': '[1, 2]'}, ['{"z":true}'])"#,
            "INSERT INTO t VALUES (2, NULL, NULL, [])",
            "CHECKPOINT",
        ] {
            database.execute(sql).expect(sql);
        }
    }
    let reopened = Database::open(name).expect("reopens");
    assert_eq!(
        rows(&reopened, sql),
        [
            vec![
                Value::Integer(1),
                Value::Varchar(r#"[{"a": 1}, NULL]"#.into()),
                Value::Varchar("[1, 2]".into()),
                Value::Varchar(r#"{"z": true}"#.into()),
            ],
            vec![Value::Integer(2), Value::Null, Value::Null, Value::Null],
        ],
        "{sql}"
    );
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}
