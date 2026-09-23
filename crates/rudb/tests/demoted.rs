//! A string column whose dictionary stopped taking values partway through a load.
//!
//! Section 5.5 of the encoding spec. A URL column of a log repeats a few values for its first hours
//! and then turns into a column of values never seen before. The writer keeps coding the first
//! stripes against the column's dictionary, and once a stripe is nearly all new values it stops
//! growing the dictionary and writes the rest of the column as plain pages. The file then holds one
//! column in two forms, and the dictionary covers only the first.
//!
//! So everything that trusted the dictionary to be the whole column is wrong for it: a distinct
//! count, a minimum and a maximum from its first and last entries, a filter answered by looking a
//! value up in it, a group key or a sort key taken from its codes. Each test here writes such a
//! table to a file, opens it again, and compares every answer with the same rows held in memory.

use rudb::Database;
use rudb_catalog::QualifiedName;
use rudb_catalog::table::Rows;

/// Rows in the table, a few stripes' worth.
const ROWS: u64 = 600_000;

/// Rows before `url` turns unique, less than one stripe so the second stripe is all new values.
const REPEATING: u64 = 100_000;

/// The statements that build the table: `url` repeats twenty values for its first rows and never
/// repeats after, and `city` repeats throughout.
///
/// `url` has no nulls, because a column with a null never answers its extremes from its dictionary
/// and that is one of the answers this is here to check. With `ty` set to `BLOB` the same values
/// are stored as bytes, which are coded against a dictionary the same way.
fn setup(ty: &str) -> Vec<String> {
    vec![
        "SET threads = 1".to_owned(),
        format!(
            "CREATE TABLE t AS SELECT i AS id, \
             (CASE WHEN i < {REPEATING} THEN 'https://example.com/' || (i % 20)::VARCHAR \
                  ELSE 'https://example.com/page/' || i::VARCHAR END)::{ty} AS url, \
             'city ' || (i % 13)::VARCHAR AS city \
             FROM range(0, {ROWS}) AS r(i)"
        ),
    ]
}

/// The table written to a file by one database and opened by another.
fn reopened(ty: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("rudb-demoted-{ty}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    let writing = Database::open(name).expect("a native database");
    for sql in setup(ty) {
        writing.execute(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    writing.execute("CHECKPOINT").expect("the table is committed");
    drop(writing);
    let database = Database::open(name).expect("the file opens");
    database.execute("SET threads = 1").expect("one thread");
    (database, path)
}

/// The same rows with no file at all, which is the answer to compare with.
fn in_memory(ty: &str) -> Database {
    let database = Database::new();
    for sql in setup(ty) {
        database.execute(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// Every row of the answer as one string per row.
fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| {
            (0..result.names().len())
                .map(|column| format!("{:?}", result.value_at(row, column)))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Whether the writer demoted a column of `t`, read off the file's directory.
fn demoted(database: &Database, column: usize) -> bool {
    database.with_catalog(|catalog| {
        let name = QualifiedName::new("memory".to_owned(), "main".to_owned(), "t".to_owned());
        match catalog.table(&name).expect("the table").rows() {
            Rows::Native(reader) => reader.demoted(column),
            Rows::Memory(_) | Rows::Grown(_, _) => panic!("the table is not in a file"),
        }
    })
}

#[test]
fn a_demoted_column_answers_every_query_as_the_rows_in_memory_do() {
    let (file, path) = reopened("VARCHAR");
    assert!(demoted(&file, 1), "url turned unique in its second stripe and was demoted");
    assert!(!demoted(&file, 2), "city kept its dictionary");
    let memory = in_memory("VARCHAR");

    let queries = [
        // Grouping on the column, where a code is not a group once the codes stop covering it.
        "SELECT url, count(*) FROM t WHERE id < 150000 GROUP BY url ORDER BY url LIMIT 40"
            .to_owned(),
        "SELECT count(*) FROM (SELECT url FROM t GROUP BY url)".to_owned(),
        "SELECT city, count(DISTINCT url) FROM t GROUP BY city ORDER BY city".to_owned(),
        // A distinct count and the extremes, which the dictionary alone would answer too small.
        "SELECT count(DISTINCT url) FROM t".to_owned(),
        "SELECT min(url), max(url) FROM t".to_owned(),
        // Filters on a value from each form of the column and one that is in neither.
        "SELECT count(*) FROM t WHERE url = 'https://example.com/7'".to_owned(),
        "SELECT id FROM t WHERE url = 'https://example.com/page/599999'".to_owned(),
        "SELECT id FROM t WHERE url = 'https://example.com/page/450000'".to_owned(),
        "SELECT count(*) FROM t WHERE url = 'https://example.com/nowhere'".to_owned(),
        "SELECT count(*) FROM t WHERE url IN ('https://example.com/3', \
         'https://example.com/page/300001')"
            .to_owned(),
        "SELECT count(*) FROM t WHERE url LIKE 'https://example.com/page/5999%'".to_owned(),
        "SELECT count(*) FROM t WHERE url LIKE '%/1_'".to_owned(),
        "SELECT count(*) FROM t WHERE url > 'https://example.com/page/5'".to_owned(),
        // A sort on the column, which a rank from the dictionary would get wrong.
        "SELECT id, url FROM t ORDER BY url, id LIMIT 30".to_owned(),
        "SELECT id, url FROM t ORDER BY url DESC, id LIMIT 30".to_owned(),
        "SELECT id, url FROM t WHERE id % 1000 = 1 ORDER BY url LIMIT 20 OFFSET 200".to_owned(),
    ];
    for sql in &queries {
        assert_eq!(rows(&file, sql), rows(&memory, sql), "{sql}");
    }
    // Pinned as well, so that the two agreeing on a wrong answer would still fail.
    assert_eq!(
        rows(&file, "SELECT count(DISTINCT url) FROM t"),
        [format!("BigInt({})", 20 + ROWS - REPEATING)]
    );
    assert_eq!(
        rows(&file, "SELECT min(url), max(url) FROM t"),
        ["Varchar(\"https://example.com/0\")|Varchar(\"https://example.com/page/599999\")"]
    );
    drop(file);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_demoted_blob_column_answers_as_the_rows_in_memory_do() {
    let (file, path) = reopened("BLOB");
    assert!(demoted(&file, 1), "url turned unique in its second stripe and was demoted");
    assert!(!demoted(&file, 2), "city kept its dictionary");
    let memory = in_memory("BLOB");

    let queries = [
        "SELECT url, count(*) FROM t WHERE id < 150000 GROUP BY url ORDER BY url LIMIT 40",
        "SELECT count(*) FROM (SELECT url FROM t GROUP BY url)",
        "SELECT count(DISTINCT url) FROM t",
        "SELECT min(url), max(url) FROM t",
        "SELECT count(*) FROM t WHERE url = 'https://example.com/7'::BLOB",
        "SELECT id FROM t WHERE url = 'https://example.com/page/450000'::BLOB",
        "SELECT count(*) FROM t WHERE url = 'https://example.com/nowhere'::BLOB",
        "SELECT id, url FROM t ORDER BY url, id LIMIT 30",
        "SELECT id, url FROM t ORDER BY url DESC, id LIMIT 30",
    ];
    for sql in queries {
        assert_eq!(rows(&file, sql), rows(&memory, sql), "{sql}");
    }
    assert_eq!(
        rows(&file, "SELECT count(DISTINCT url) FROM t"),
        [format!("BigInt({})", 20 + ROWS - REPEATING)]
    );
    drop(file);
    let _ = std::fs::remove_file(path);
}
