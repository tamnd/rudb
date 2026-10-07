//! A type `CREATE TYPE` made in a file, through a close and a reopen.
//!
//! The file had nowhere to keep one, so `CREATE TYPE` in a database file was refused. A made type
//! now goes into the file's list of views as the type it stands for, and every answer below was
//! taken from the pinned duckdb binary, v2.0.0-dev84237, over the same statements.

use rudb::Database;

/// Every row of `sql` as the shell writes it.
fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| {
            (0..result.width())
                .map(|column| result.text_at(row, column))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

const MADE: &str = "SELECT type_name, logical_type FROM duckdb_types() WHERE NOT internal \
                    ORDER BY type_name";

#[test]
fn a_made_type_comes_back_with_the_file() {
    let path = std::env::temp_dir().join(format!("rudb-type-stored-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");

    {
        let database = Database::open(name).expect("a file name starts a native database");
        for sql in [
            "CREATE TYPE mood AS ENUM ('sad', 'it''s ok', 'happy')",
            "CREATE TYPE pair AS STRUCT(m mood, n INT)",
            "CREATE TYPE word AS VARCHAR",
            "CREATE TYPE picked AS ENUM (SELECT 'b' UNION ALL SELECT 'a')",
            "CREATE TABLE t (m mood, p pair, w word)",
            "INSERT INTO t VALUES ('happy', {'m': 'sad', 'n': 1}, 'x')",
            "CREATE MACRO f(x mood) AS x::VARCHAR || '!'",
        ] {
            database.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
    }

    {
        let database = Database::open(name).expect("reopens");
        assert_eq!(
            answered(&database, MADE),
            ["mood|ENUM", "pair|STRUCT", "picked|ENUM", "word|VARCHAR"]
        );
        let sql = "SELECT 'happy'::mood < 'sad'::mood, 'b'::picked < 'a'::picked, f('sad'::mood)";
        assert_eq!(answered(&database, sql), ["false|true|sad!"], "{sql}");
        assert_eq!(answered(&database, "SELECT * FROM t"), ["happy|{'m': sad, 'n': 1}|x"]);
        let sql = "SELECT column_type FROM (DESCRIBE t)";
        assert_eq!(
            answered(&database, sql),
            [
                "ENUM('sad', 'it''s ok', 'happy')",
                "STRUCT(m ENUM('sad', 'it''s ok', 'happy'), n INTEGER)",
                "VARCHAR"
            ],
            "{sql}"
        );
        let error = database.execute("DROP TYPE mood").expect_err("pair was made from mood");
        assert_eq!(
            error.to_string(),
            "Dependency Error: Cannot drop entry \"mood\" because there are entries that depend \
             on it.\ntype \"pair\" depends on type \"mood\".\nUse DROP...CASCADE to drop all \
             dependents."
        );
        database.execute("DROP TYPE pair").expect("nothing was made from pair");
        database.execute("DROP TYPE mood").expect("nothing is made from mood now");
        assert_eq!(answered(&database, MADE), ["picked|ENUM", "word|VARCHAR"]);
        assert_eq!(answered(&database, "SELECT * FROM t"), ["happy|{'m': sad, 'n': 1}|x"]);
    }

    let database = Database::open(name).expect("reopens again");
    assert_eq!(answered(&database, MADE), ["picked|ENUM", "word|VARCHAR"]);
    database.execute("CREATE TYPE mood AS INT").expect("the name is free again");
    assert_eq!(answered(&database, "SELECT 1::mood + 1"), ["2"]);
    drop(database);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_column_of_a_table_in_another_database_reads_a_bare_type_name_there_first() {
    let first =
        std::env::temp_dir().join(format!("rudb-type-attached-{}.rudb", std::process::id()));
    let second =
        std::env::temp_dir().join(format!("rudb-type-attached-{}-2.rudb", std::process::id()));
    let _ = std::fs::remove_file(&first);
    let _ = std::fs::remove_file(&second);
    let (first_name, second_name) = (first.display(), second.display());

    let database = Database::new();
    for sql in [
        format!("ATTACH '{first_name}' AS db1"),
        "CREATE TYPE db1.mood AS ENUM ('sad', 'ok', 'happy')".to_string(),
        "CREATE TABLE db1.person (name text, current_mood mood)".to_string(),
        "INSERT INTO db1.person VALUES ('Moe', 'happy')".to_string(),
        "DETACH db1".to_string(),
        format!("ATTACH '{first_name}' AS db1 (READ_ONLY)"),
        format!("ATTACH '{second_name}' AS db2"),
        "CREATE TYPE db2.mood AS ENUM ('ble', 'grr', 'kkcry')".to_string(),
        "CREATE TABLE db2.person (name text, current_mood mood)".to_string(),
        "INSERT INTO db2.person VALUES ('Moe', 'kkcry')".to_string(),
    ] {
        database.execute(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    assert_eq!(answered(&database, "SELECT enum_range(NULL::db1.main.mood)"), ["[sad, ok, happy]"]);
    assert_eq!(answered(&database, "SELECT * FROM db1.person"), ["Moe|happy"]);
    assert_eq!(answered(&database, "SELECT * FROM db2.person"), ["Moe|kkcry"]);
    let error = database.execute("SELECT NULL::xx.db1.main.mood").expect_err("four parts");
    assert_eq!(
        error.to_string(),
        "Catalog Error: Type with name \"xx.db1.main.mood\" does not exist because schema \
         \"xx.db1.main\" does not exist."
    );
    drop(database);
    let _ = std::fs::remove_file(&first);
    let _ = std::fs::remove_file(&second);
}
