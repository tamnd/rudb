//! What a table on disk says about the form its columns are stored in.
//!
//! `pragma_storage_info` is the only metadata table whose rows come out of the data. Everything the
//! other pragmas report is in memory before the query starts, and this one opens the column pages
//! and reads the header of every part of every column, because what the encoder chose is written
//! there and nowhere else. So it is the one that cannot be tested without a file, which is why it
//! has a test of its own out here rather than a case in the in memory pragma tests.
//!
//! The width of the answer and the empty answer for rows that are still in memory are asserted in
//! `crate::tests`. What is only assertable against a file is the rest: a row per part per column
//! covering every row of the table exactly once, and a `compression` that names what the encoder
//! actually picked rather than what the schema said. The two columns here are chosen so that those
//! two answers differ from each other, since a report that said the same thing about every column
//! would pass a test that only looked at one.

use rudb::Database;
use rudb_common::Value;

/// Sixty thousand rows, which is more than one part however the parts fall.
///
/// The key climbs with an irregular step, the way a key column in arrival order does, so the
/// encoder has deltas to work with rather than a constant stride. The label takes three values, so
/// the encoder has a dictionary to find. Those are the two shapes worth telling apart and the whole
/// reason this report exists is that the file is the only place that says which one a column got.
const ROWS: &str = "SELECT (r * 3 + r % 7)::BIGINT AS key, \
                    ['north', 'south', 'east'][r % 3 + 1] AS label \
                    FROM range(60000) AS s(r)";

/// Every row of a result, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// The number in a column of the report, which every count and offset in it is.
fn number(row: &[Value], column: usize) -> i64 {
    match row[column] {
        Value::BigInt(number) => number,
        ref other => panic!("column {column} of this report is a BIGINT, not {other:?}"),
    }
}

/// The text in a column of the report.
fn text(row: &[Value], column: usize) -> String {
    match row[column] {
        Value::Varchar(ref text) => text.clone(),
        ref other => panic!("column {column} of this report is a VARCHAR, not {other:?}"),
    }
}

#[test]
fn a_checkpointed_table_reports_a_part_at_a_time_and_names_what_the_encoder_chose() {
    let path = std::env::temp_dir().join(format!("rudb-storage-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("a file name starts a native database");
    database.execute("SET threads = 1").expect("sets the thread count");
    database.execute("CREATE TABLE t (key BIGINT, label VARCHAR)").expect("creates");
    database.execute(&format!("INSERT INTO t {ROWS}")).expect("loads");

    // An insert into a native table encodes and writes its parts as it goes rather than holding
    // them until a checkpoint, so the report is already populated here. The checkpoint is run
    // anyway, because what it commits is the directory the reader walks and reading a file whose
    // directory is one version behind is a different test than this one.
    let during = rows(&database, "SELECT * FROM pragma_storage_info('t')");
    assert!(!during.is_empty(), "an insert writes its parts, so they should be reportable");
    database.execute("CHECKPOINT").expect("commits");

    let report = rows(&database, "SELECT * FROM pragma_storage_info('t')");
    assert_eq!(report, during, "a checkpoint with nothing left to move changed the report");
    // The two spellings are the same call, and unlike the other pragmas this one has rows to
    // compare, so the agreement means something here.
    assert_eq!(rows(&database, "PRAGMA storage_info('t')"), report);

    // A row per part per column, columns in schema order and parts in file order inside each, with
    // every row of the table accounted for exactly once in each column.
    for (at, (column, ty)) in [("key", "BIGINT"), ("label", "VARCHAR")].iter().enumerate() {
        let held: Vec<&Vec<Value>> = report.iter().filter(|row| text(row, 1) == *column).collect();
        assert!(held.len() > 1, "sixty thousand rows should not be one part: {}", held.len());
        let mut start = 0;
        // row at a time: a report of sixteen parts is what a test walks, not what a kernel runs on.
        for (part, row) in held.iter().enumerate() {
            let at = i64::try_from(at).expect("two columns");
            assert_eq!(number(row, 2), at, "column_id follows the schema");
            assert_eq!(text(row, 3), format!("[{at}]"));
            assert_eq!(number(row, 4), i64::try_from(part).expect("a part number"));
            assert_eq!(text(row, 5), *ty, "segment_type is the column's type");
            assert_eq!(number(row, 6), start, "a part starts where the one before it ended");
            start += number(row, 7);
            // Nothing here has been updated and all of it is on disk, and both of those will mean
            // something the day a native table has a delta region to report.
            assert_eq!(row[10], Value::Boolean(false));
            assert_eq!(row[11], Value::Boolean(true));
            assert!(text(row, 14).ends_with(" bytes"), "{}", text(row, 14));
            assert_eq!(
                row[15],
                Value::List { element: rudb_common::LogicalType::BigInt, values: Vec::new() }
            );
        }
        assert_eq!(start, 60000, "every row of {column} is in exactly one part");
    }

    // And the part this exists for. The two columns hold the same rows and the encoder made a
    // different choice about each, which is a thing only the file knows: the labels are three
    // strings repeating and come back as codes into a dictionary, and the keys climb and come back
    // as a cascade over their deltas. Neither is derivable from the schema, which is the whole
    // argument for reading the pages.
    let chosen = |column: &str| {
        report
            .iter()
            .filter(|row| text(row, 1) == column)
            .map(|row| text(row, 8))
            .next()
            .expect("the column is in the report")
    };
    let key = chosen("key");
    let label = chosen("label");
    assert!(key.contains("DELTA"), "an ascending key should delta encode, not {key}");
    assert!(label.contains("DICT"), "three repeating strings should be a dictionary, not {label}");

    // The ranges come off the file too, and they are the ones the scan prunes with, so a report
    // that disagreed with them would be reporting on some other file.
    let first = report.iter().find(|row| text(row, 1) == "key").expect("the key column");
    assert!(text(first, 9).starts_with("[Min: 0, Max: "), "{}", text(first, 9));
    assert!(text(first, 9).ends_with("[Has Null: false, Has No Null: true]"), "{}", text(first, 9));

    drop(database);
    let _ = std::fs::remove_file(&path);
}
