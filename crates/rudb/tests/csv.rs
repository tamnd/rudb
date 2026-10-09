//! `read_csv` from SQL, and the replacement scan over a `.csv` or a `.tsv` name.
//!
//! The fixtures are `rudb-csv`'s, referred to across the workspace rather than copied. The big one
//! holds the same 4096 rows as the Parquet fixture, written out by the same DuckDB query, so an
//! answer that differs between the two readers shows up as a number that differs between this file
//! and `tests/parquet.rs` rather than as nothing at all.
//!
//! What is asserted here is the SQL and the sniffing that SQL cannot see any other way. The
//! splitting and the type ladder have their own tests one layer down.

use rudb::Database;
use rudb_common::Value;

/// The path of a fixture, as a SQL string literal.
fn fixture(name: &str) -> String {
    format!("'{}/../rudb-csv/testdata/{name}'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT <what> FROM read_csv(<mixed.csv>)`, as one value.
fn one(what: &str) -> Value {
    let database = Database::new();
    let sql = format!("SELECT {what} FROM read_csv({})", fixture("mixed.csv"));
    database.value(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

#[test]
fn counting_the_rows_of_a_file_is_the_number_of_rows_in_it() {
    // 4096 and not 4097, which is what a reader that took the header line for a row would say.
    assert_eq!(one("count(*)"), Value::BigInt(4096));
}

#[test]
fn the_columns_are_the_names_and_the_types_duckdb_sniffs_for_the_same_file() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({})", fixture("mixed.csv"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 4096);
    assert_eq!(result.names(), ["a", "b", "s", "d", "flag", "day", "t"]);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    // `a` is a BIGINT and not an INTEGER, which is the one place this disagrees with the Parquet
    // fixture holding the same numbers. A CSV file does not say how wide its integers are and the
    // sniffer's rung is BIGINT, so this is DuckDB's answer rather than a loss of information.
    assert_eq!(types, ["BIGINT", "BIGINT", "VARCHAR", "DOUBLE", "BOOLEAN", "DATE", "TIMESTAMP"]);
}

#[test]
fn the_first_row_is_the_one_duckdb_reads_from_the_same_bytes() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({}) LIMIT 1", fixture("mixed.csv"));
    let result = database.query(&sql).expect("runs");
    let row: Vec<Value> = (0..result.width()).map(|at| result.value_at(0, at)).collect();
    assert_eq!(
        row,
        vec![
            Value::BigInt(0),
            Value::BigInt(0),
            Value::Null,
            Value::Double(0.0),
            Value::Boolean(true),
            Value::Date(0),
            Value::Timestamp(1_373_882_400_000_000),
        ]
    );
}

#[test]
fn the_sums_are_the_sums_duckdb_reports_for_the_same_file() {
    assert_eq!(one("sum(a)"), Value::HugeInt(195_783));
    assert_eq!(one("sum(b)"), Value::HugeInt(2_002_560_000));
    assert_eq!(one("sum(d)"), Value::Double(193_536.0));
    assert_eq!(one("min(day)"), Value::Date(0));
    assert_eq!(one("max(day)"), Value::Date(999));
}

#[test]
fn an_empty_field_is_a_null_and_not_an_empty_string() {
    // The one place a CSV file is ambiguous and DuckDB picks a side. 586 of the 4096 rows have
    // nothing between the two commas, and every one of them is a null.
    assert_eq!(one("count(s)"), Value::BigInt(3510));
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_csv({}) WHERE s IS NULL", fixture("mixed.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(586));
}

#[test]
fn a_where_clause_over_a_file_filters_the_rows_it_read() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_csv({}) WHERE flag", fixture("mixed.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(2048));
    let sql = format!("SELECT count(*) FROM read_csv({}) WHERE a < 10", fixture("mixed.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(430));
}

#[test]
fn an_aggregate_over_a_text_column_groups_by_its_values() {
    let database = Database::new();
    let sql = format!(
        "SELECT s, count(*) FROM read_csv({}) WHERE s IS NOT NULL GROUP BY s ORDER BY s",
        fixture("mixed.csv")
    );
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 5, "tag0 to tag4");
    assert_eq!(result.value_at(0, 0), Value::Varchar("tag0".into()));
}

#[test]
fn read_csv_auto_reads_the_same_file_under_duckdbs_older_name_for_the_function() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_csv_auto({})", fixture("mixed.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(4096));
}

#[test]
fn an_alias_renames_the_call_and_not_its_columns() {
    let database = Database::new();
    let sql = format!("SELECT c.a FROM read_csv({}) AS c LIMIT 1", fixture("mixed.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(0));
}

#[test]
fn a_file_with_no_header_gets_the_names_duckdb_gives_it() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({})", fixture("noheader.csv"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.names(), ["column0", "column1", "column2"]);
    assert_eq!(result.len(), 3);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["BIGINT", "VARCHAR", "DOUBLE"]);
}

#[test]
fn a_tab_separated_file_is_worked_out_from_its_bytes_and_not_from_its_name() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({})", fixture("punctuation.tsv"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.names(), ["name", "note"]);
    assert_eq!(result.len(), 3);
    assert_eq!(result.value_at(0, 1), Value::Varchar("holds\ta tab".into()));
    assert_eq!(result.value_at(1, 1), Value::Varchar("holds\na newline".into()));
    assert_eq!(result.value_at(2, 1), Value::Varchar("says \"hi\"".into()));
}

#[test]
fn a_file_that_is_not_there_fails_at_bind_time_with_duckdbs_message() {
    let database = Database::new();
    let error = database.query("SELECT * FROM read_csv('/nowhere/at/all.csv')").unwrap_err();
    assert_eq!(error.message(), "No files found that match the pattern \"/nowhere/at/all.csv\"");
}

#[test]
fn a_path_that_is_not_a_string_does_not_become_one() {
    let database = Database::new();
    let error = database.query("SELECT * FROM read_csv(3)").unwrap_err();
    assert!(error.message().contains("read_csv(INTEGER)"), "{error}");
}

#[test]
fn a_csv_name_where_a_table_name_goes_reads_the_file() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM {}", fixture("mixed.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(4096));
}

#[test]
fn a_tsv_name_goes_to_the_same_reader_as_a_csv_one() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM {}", fixture("punctuation.tsv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(3));
}

#[test]
fn the_columns_of_a_replaced_file_answer_to_the_stem_of_its_name() {
    let database = Database::new();
    let sql = format!("SELECT mixed.a FROM {} LIMIT 1", fixture("mixed.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(0));
}

#[test]
fn a_csv_file_that_is_not_there_is_the_file_error_and_not_the_table_one() {
    // The extension is read before the file is looked for, so a name that says csv gets the
    // reader's complaint about the file rather than the catalog's complaint about the name.
    let database = Database::new();
    let error = database.query("SELECT * FROM '/nowhere/at/all.csv'").unwrap_err();
    assert_eq!(error.message(), "No files found that match the pattern \"/nowhere/at/all.csv\"");
}

#[test]
fn a_file_that_is_there_and_has_an_extension_nothing_reads_says_so() {
    // The README of the fixture directory, which exists and is not a file any reader here takes.
    // A name with the same extension that is not there is the catalog error instead, which is the
    // other half of the rule and is asserted next to it.
    let database = Database::new();
    let sql = format!("SELECT * FROM {}", fixture("README.md"));
    let error = database.query(&sql).unwrap_err();
    assert!(
        error.message().starts_with("No extension found that is capable of reading the file"),
        "{error}"
    );
    assert!(error.message().contains("read_csv, read_json or read_parquet"), "{error}");
    let error = database.query("SELECT * FROM 'nowhere.md'").unwrap_err();
    assert_eq!(error.message(), "Table with name nowhere.md does not exist!");
}

#[test]
fn a_file_can_be_loaded_into_a_table_and_queried_from_there() {
    let database = Database::new();
    let sql = format!("CREATE TABLE loaded AS SELECT a, s FROM read_csv({})", fixture("mixed.csv"));
    database.execute(&sql).expect("loads");
    assert_eq!(database.value("SELECT count(*) FROM loaded").expect("runs"), Value::BigInt(4096));
    assert_eq!(database.value("SELECT sum(a) FROM loaded").expect("runs"), Value::HugeInt(195_783));
    assert_eq!(database.value("SELECT count(s) FROM loaded").expect("runs"), Value::BigInt(3510));
}

#[test]
fn a_call_that_says_there_is_no_header_reads_the_first_line_as_a_row() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({}, header=false)", fixture("mixed.csv"));
    let result = database.query(&sql).expect("runs");
    // 4097 and not 4096, because the line that was the header is a row now, and every column is
    // VARCHAR because that line holds `a`, `b` and `s` where the numbers were.
    assert_eq!(result.len(), 4097);
    assert_eq!(
        result.names(),
        ["column0", "column1", "column2", "column3", "column4", "column5", "column6"]
    );
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["VARCHAR"; 7]);
    assert_eq!(result.value_at(0, 0), Value::Varchar("a".into()));
    assert_eq!(result.value_at(1, 0), Value::Varchar("0".into()));
}

#[test]
fn a_call_that_says_there_is_a_header_takes_the_first_line_for_the_names() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({}, header=true)", fixture("noheader.csv"));
    let result = database.query(&sql).expect("runs");
    // The file has no header, so this is the caller being wrong on purpose, and DuckDB does what it
    // was told: the first line becomes three column names and the two rows left are the read.
    assert_eq!(result.names(), ["1", "x", "2.5"]);
    assert_eq!(result.len(), 2);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["BIGINT", "VARCHAR", "DOUBLE"]);
}

#[test]
fn a_given_delimiter_is_the_delimiter_the_sniffer_would_not_have_picked() {
    let database = Database::new();
    let sniffed = format!("SELECT * FROM read_csv({})", fixture("given/semicolon.csv"));
    let result = database.query(&sniffed).expect("runs");
    assert_eq!(result.names(), ["name", "x;note", "y"]);
    assert_eq!(result.value_at(0, 1), Value::Varchar("b;c".into()));
    for parameter in ["delim", "sep"] {
        let sql =
            format!("SELECT * FROM read_csv({}, {parameter}=';')", fixture("given/semicolon.csv"));
        let result = database.query(&sql).expect("runs");
        assert_eq!(result.names(), ["name,x", "note,y"], "{parameter}");
        assert_eq!(result.len(), 2, "{parameter}");
        assert_eq!(result.value_at(0, 0), Value::Varchar("a,b".into()), "{parameter}");
        assert_eq!(result.value_at(1, 1), Value::Varchar("g,h".into()), "{parameter}");
    }
}

#[test]
fn a_given_quote_is_stripped_off_a_value_the_sniffer_would_have_kept() {
    let database = Database::new();
    let sniffed = format!("SELECT * FROM read_csv({})", fixture("given/hashquote.csv"));
    let result = database.query(&sniffed).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::Varchar("#one#".into()));
    let sql = format!("SELECT * FROM read_csv({}, quote='#')", fixture("given/hashquote.csv"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.names(), ["name", "note"]);
    assert_eq!(result.value_at(0, 0), Value::Varchar("one".into()));
    assert_eq!(result.value_at(1, 0), Value::Varchar("two".into()));
}

#[test]
fn a_given_escape_puts_the_quote_byte_inside_the_value() {
    let database = Database::new();
    let sql =
        format!("SELECT * FROM read_csv({}, quote='#', escape='\\')", fixture("given/escaped.csv"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::Varchar("a#b".into()));
    assert_eq!(result.value_at(1, 0), Value::Varchar("c#d".into()));
    assert_eq!(result.value_at(0, 1), Value::Varchar("x".into()));
}

#[test]
fn all_varchar_keeps_the_names_the_sniffer_found_and_throws_away_the_types() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({}, all_varchar=true)", fixture("mixed.csv"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 4096);
    assert_eq!(result.names(), ["a", "b", "s", "d", "flag", "day", "t"]);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["VARCHAR"; 7]);
    assert_eq!(result.value_at(1, 3), Value::Varchar("1.5".into()));
    assert_eq!(result.value_at(1, 5), Value::Varchar("1970-01-02".into()));
}

#[test]
fn a_named_parameter_read_csv_does_not_take_lists_the_ones_it_does() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({}, nosuch=1)", fixture("mixed.csv"));
    let error = database.query(&sql).unwrap_err();
    // The layout is the binary's: the name on its own line, then one indented `name TYPE` per
    // candidate in alphabetical order. The list here is shorter than DuckDB's forty seven, because
    // a parameter that is listed is one that does something.
    let expected = concat!(
        "Invalid named parameter \"nosuch\" for function read_csv\n",
        "Candidates:\n",
        "    all_varchar BOOLEAN\n",
        "    auto_detect BOOLEAN\n",
        "    column_names VARCHAR[]\n",
        "    columns ANY\n",
        "    delim VARCHAR\n",
        "    escape VARCHAR\n",
        "    header BOOLEAN\n",
        "    names VARCHAR[]\n",
        "    nullstr VARCHAR\n",
        "    quote VARCHAR\n",
        "    sep VARCHAR\n",
    );
    assert_eq!(error.message(), expected);
}

/// A file of these bytes under a name of its own, as a SQL string literal.
fn written(name: &str, text: &str) -> String {
    let path = format!("{}/csv-{name}", env!("CARGO_TARGET_TMPDIR"));
    std::fs::write(&path, text).expect("writes");
    format!("'{path}'")
}

#[test]
fn columns_names_and_types_the_file_and_the_types_decide_the_header() {
    let database = Database::new();
    let header = written("columns-header.csv", "a,b\n1,x\n2,y\n");
    let numbers = written("columns-numbers.csv", "1,2\n3,4\n");
    let one_line = written("columns-one-line.csv", "1,2\n");
    let read = |file: &str, columns: &str| {
        let sql = format!("SELECT * FROM read_csv({file}, columns={columns})");
        database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
    };
    // `a` is not an integer, so the first line is a header and the names are the call's.
    let result = read(&header, "{'p': 'INT', 'q': 'VARCHAR'}");
    assert_eq!(result.names(), ["p", "q"]);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["INTEGER", "VARCHAR"]);
    assert_eq!(result.len(), 2);
    assert_eq!(result.value_at(0, 0), Value::Integer(1));
    // `1,2` fits an integer, so it is a row, and over text columns it is a header, because a file
    // whose columns are all text has one. A single line is asked the same question.
    assert_eq!(read(&numbers, "{'p': 'INT', 'q': 'VARCHAR'}").len(), 2);
    let text = read(&numbers, "{'p': 'VARCHAR', 'q': 'VARCHAR'}");
    assert_eq!(text.len(), 1);
    assert_eq!(text.value_at(0, 0), Value::Varchar("3".into()));
    assert_eq!(read(&one_line, "{'p': 'VARCHAR', 'q': 'VARCHAR'}").len(), 0);
    // The pin's widths for a bare `DECIMAL`, and `all_varchar` leaves set types alone.
    let sql = format!(
        "SELECT * FROM read_csv({numbers}, columns={{'p': 'DECIMAL', 'q': 'INT'}}, \
         all_varchar=true)"
    );
    let result = database.query(&sql).expect("runs");
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["DECIMAL(18,3)", "INTEGER"]);
}

#[test]
fn auto_detect_false_sniffs_nothing_and_wants_the_columns_set() {
    let database = Database::new();
    let header = written("fixed-header.csv", "a,b\n1,x\n2,y\n");
    let short = written("fixed-short.csv", "1,x\n2,y\n");
    let sql = format!(
        "SELECT * FROM read_csv({header}, columns={{'p': 'VARCHAR', 'q': 'VARCHAR'}}, \
         auto_detect=false)"
    );
    let result = database.query(&sql).expect("runs");
    // Nothing decided the first line was a header, so it is a row.
    assert_eq!(result.len(), 3);
    assert_eq!(result.value_at(0, 0), Value::Varchar("a".into()));
    let error = database
        .query(&format!("SELECT * FROM read_csv({header}, auto_detect=false)"))
        .unwrap_err();
    assert_eq!(
        error.message(),
        "read_csv requires columns to be specified through the 'columns' option. Use \
         read_csv_auto or set read_csv(..., AUTO_DETECT=TRUE) to automatically guess columns."
    );
    let sql = format!(
        "SELECT * FROM read_csv({short}, columns={{'p': 'INT', 'q': 'VARCHAR', 'r': 'INT'}}, \
         auto_detect=false)"
    );
    let error = database.query(&sql).unwrap_err();
    assert!(
        error.message().starts_with(
            "CSV Error on Line: 1\nOriginal Line: 1,x\nExpected Number of Columns: 3 Found: 2\n"
        ),
        "{error}"
    );
}

#[test]
fn columns_that_are_not_a_struct_of_type_names_are_refused_in_the_pins_words() {
    let database = Database::new();
    let file = written("refused.csv", "1,2\n3,4\n");
    for (columns, expected) in [
        ("42", "read_csv columns requires a struct as input"),
        ("NULL", "read_csv columns requires a struct as input"),
        ("{}", "read_csv requires at least a single column as input!"),
        ("{'p': 5}", "read_csv requires a type specification as string"),
        ("{'p': 'INT', 'q': NULL}", "read_csv requires a type specification as string"),
    ] {
        let sql = format!("SELECT * FROM read_csv({file}, columns={columns})");
        let error = database.query(&sql).unwrap_err();
        assert_eq!(error.message(), expected, "{columns}");
    }
    let sql = format!("SELECT * FROM read_csv({file}, columns={{'p': 'NOTATYPE', 'q': 'INT'}})");
    let error = database.query(&sql).unwrap_err();
    assert!(error.message().starts_with("Type with name NOTATYPE does not exist!"), "{error}");
    for names in ["names", "column_names"] {
        let sql = format!("SELECT * FROM read_csv({file}, columns={{'p': 'INT'}}, {names}=['a'])");
        let error = database.query(&sql).unwrap_err();
        assert_eq!(error.message(), "read_csv column_names/names can only be supplied once");
    }
    let sql =
        format!("SELECT * FROM read_csv({file}, columns={{'p': 'INT', 'q': 'INT'}}, delim=';')");
    let error = database.query(&sql).unwrap_err();
    assert!(error.message().starts_with("Error when sniffing file"), "{error}");
    assert!(
        error.message().ends_with(
            "\"columns = { 'p' : 'INTEGER', 'q' : 'INTEGER'}\", and they contain: 2 columns. It \
             does not match the number of columns found by the sniffer: 1. Verify the columns \
             parameter is correctly set."
        ),
        "{error}"
    );
    let sql = format!("SELECT * FROM read_csv({file}, auto_detect=NULL)");
    let error = database.query(&sql).unwrap_err();
    assert_eq!(
        error.message(),
        "\"auto_detect\" expects a non-null boolean value (e.g. TRUE or 1)"
    );
}
