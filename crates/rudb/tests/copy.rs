//! `COPY t FROM 'file'`, which is `INSERT INTO t SELECT * FROM read_csv('file', ...)` read as the
//! table's types.
//!
//! The file the first test loads is written in the shape the Join Order Benchmark's are, and the
//! statement is the one its loader runs, so the rows it asserts are the rows DuckDB
//! `v2.0.0-dev84237` loaded from the same bytes with the same statement. Each file is written to
//! the temporary directory under a name with the process in it, and removed again at the end.

use rudb::Database;
use rudb_common::Value;

/// A file holding `text`, removed when this goes out of scope, so a test that fails still cleans
/// up after itself.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str, text: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("rudb-copy-{}-{name}", std::process::id()));
        std::fs::write(&path, text).expect("writes the file");
        Scratch(path)
    }

    fn path(&self) -> &str {
        self.0.to_str().expect("a UTF-8 temporary path")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn text(value: &str) -> Value {
    Value::Varchar(value.into())
}

/// Every row of `sql`, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|at| result.value_at(row, at)).collect())
        .collect()
}

#[test]
fn the_join_order_benchmark_statement_loads_its_file_the_way_duckdb_does() {
    // A backslash escape inside quotes, an empty field in an integer column and in a text column,
    // a quoted field holding a comma and a newline, and a quoted empty string, which `NULL ''`
    // makes a null as well.
    let file = Scratch::new(
        "job.csv",
        "1,\"O\\\"Brien, Pat\",plain,5\n2,,\"a,b\nc\",\n3,\"back\\\\slash\",\"\",7\n",
    );
    let database = Database::new();
    database
        .execute("CREATE TABLE name (id INTEGER, name VARCHAR, note VARCHAR, n INTEGER)")
        .expect("creates");
    let sql = format!(
        "COPY name FROM '{}' (FORMAT csv, HEADER false, ESCAPE '\\', QUOTE '\"', NULL '')",
        file.path()
    );
    let result = database.execute(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    assert_eq!(result.names(), ["Count"]);
    assert_eq!(result.value_at(0, 0), Value::BigInt(3));
    assert_eq!(
        rows(&database, "SELECT * FROM name ORDER BY id"),
        [
            vec![Value::Integer(1), text("O\"Brien, Pat"), text("plain"), Value::Integer(5)],
            vec![Value::Integer(2), Value::Null, text("a,b\nc"), Value::Null],
            vec![Value::Integer(3), text("back\\slash"), Value::Null, Value::Integer(7)],
        ]
    );
}

#[test]
fn a_column_list_puts_the_files_columns_in_those_columns_and_the_rest_get_their_defaults() {
    let file = Scratch::new("list.csv", "x,1\ny,2\n");
    let database = Database::new();
    database
        .execute("CREATE TABLE t (a INTEGER, b VARCHAR, c INTEGER DEFAULT 9)")
        .expect("creates");
    let sql = format!("COPY t (b, a) FROM '{}' (HEADER false)", file.path());
    database.execute(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    assert_eq!(
        rows(&database, "SELECT * FROM t ORDER BY a"),
        [
            vec![Value::Integer(1), text("x"), Value::Integer(9)],
            vec![Value::Integer(2), text("y"), Value::Integer(9)],
        ]
    );
}

#[test]
fn a_header_is_sniffed_when_the_statement_does_not_say_and_skipped_when_it_does() {
    let file = Scratch::new("header.csv", "k|v\n1|one\n2|two\n");
    let database = Database::new();
    database.execute("CREATE TABLE t (k BIGINT, v VARCHAR)").expect("creates");
    let sniffed = format!("COPY t FROM '{}' (DELIMITER '|')", file.path());
    database.execute(&sniffed).expect("loads");
    let said = format!("COPY t FROM '{}' WITH DELIMITER AS '|' CSV HEADER", file.path());
    database.execute(&said).expect("loads");
    assert_eq!(database.value("SELECT count(*) FROM t").expect("runs"), Value::BigInt(4));
    assert_eq!(database.value("SELECT sum(k) FROM t").expect("runs"), Value::HugeInt(6));
}

#[test]
fn a_value_that_does_not_fit_the_table_is_duckdbs_conversion_error_on_its_line() {
    let file = Scratch::new("bad.csv", "1,x\nfoo,y\n");
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    let sql = format!("COPY t FROM '{}' (HEADER false, ESCAPE '\\', QUOTE '\"')", file.path());
    let error = database.execute(&sql).unwrap_err().to_string();
    assert!(error.starts_with("Conversion Error: CSV Error on Line: 2\n"), "{error}");
    assert!(
        error.contains(
            "Error when converting column \"a\". Could not convert string \"foo\" to 'INTEGER'\n\n\
             Column a is being converted as type INTEGER\n\
             This type was either manually set or derived from an existing table. Select a \
             different type to correctly parse this column.\n"
        ),
        "{error}"
    );
    assert!(error.contains("escape = \\ (Set By User)"), "{error}");
    assert_eq!(database.value("SELECT count(*) FROM t").expect("runs"), Value::BigInt(0));
}

#[test]
fn a_file_with_a_different_number_of_columns_is_refused() {
    let file = Scratch::new("wide.csv", "1,x,z\n");
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    let error = database.execute(&format!("COPY t FROM '{}'", file.path())).unwrap_err();
    let error = error.to_string();
    assert!(error.starts_with("Invalid Input Error: "), "{error}");
    assert!(
        error.contains(
            "* Columns are set as: \"columns = { 'a' : 'INTEGER', 'b' : 'VARCHAR'}\", and they \
             contain: 2 columns. It does not match the number of columns found by the sniffer: 3."
        ),
        "{error}"
    );
}

#[test]
fn a_copy_that_cannot_happen_says_what_duckdb_says() {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    database.execute("CREATE VIEW v AS SELECT 1 AS a").expect("creates the view");
    let missing = database.execute("COPY t FROM '/nowhere/at/all.csv'").unwrap_err();
    assert_eq!(
        missing.to_string(),
        "IO Error: No files found that match the pattern \"/nowhere/at/all.csv\""
    );
    let view = database.execute("COPY v FROM '/nowhere/at/all.csv'").unwrap_err();
    assert_eq!(view.to_string(), "Catalog Error: v is not an table");
    let option = database.execute("COPY t FROM 'x.csv' (FOO 1)").unwrap_err();
    assert!(option.to_string().starts_with("Not implemented Error: Unrecognized option \"foo\""));
}

#[test]
fn read_csv_takes_a_null_string_or_a_list_of_them() {
    let file = Scratch::new("nulls.csv", "a,b\n1,NA\n-,x\n,\"\"\n");
    let database = Database::new();
    let one =
        format!("SELECT a, b FROM read_csv('{}', nullstr = 'NA', all_varchar = true)", file.path());
    assert_eq!(
        rows(&database, &one),
        [vec![text("1"), Value::Null], vec![text("-"), text("x")], vec![text(""), text("")]]
    );
    let two = format!(
        "SELECT count(a), count(b) FROM read_csv('{}', nullstr = ['NA', '-'])",
        file.path()
    );
    assert_eq!(rows(&database, &two), [vec![Value::BigInt(2), Value::BigInt(2)]]);
    let wrong = format!("SELECT * FROM read_csv('{}', nullstr = 1)", file.path());
    let error = database.query(&wrong).unwrap_err().to_string();
    assert!(
        error.ends_with(
            "CSV Reader function option \"nullstr\" requires a string or a list as input"
        ),
        "{error}"
    );
}
