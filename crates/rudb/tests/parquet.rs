//! `read_parquet` from SQL, which is the first query rudb answers out of a file on disk, and the
//! replacement scan that lets the same file be written where a table name goes.
//!
//! The fixture is `rudb-parquet`'s, referred to across the workspace rather than copied, because two
//! copies of a binary file are two things to keep in step and a test that reads the stale one passes
//! while meaning nothing. It is 4096 rows in two row groups, written by DuckDB, and what each column
//! holds is the closed form checked against DuckDB in `crates/rudb-parquet/tests/read.rs`.
//!
//! What is asserted here is the SQL, not the decoding. The decoding has its own test one layer down
//! and repeating it here would be the same assertions twice with a parser in front of them.

use rudb::Database;
use rudb_common::{Stage, Value};

/// The path of the fixture, as a SQL string literal.
fn fixture() -> String {
    format!("'{}/../rudb-parquet/testdata/mixed.parquet'", env!("CARGO_MANIFEST_DIR"))
}

/// The path of the byte array fixture, as a SQL string literal.
///
/// Two `binary` columns written by pyarrow, which arrive as `BYTE_ARRAY` with no logical type on
/// them. That is the shape every string column in the ClickBench file has and it is the reason
/// `binary_as_string` exists.
fn bytes() -> String {
    format!("'{}/../rudb-parquet/testdata/bytes.parquet'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT <what> FROM read_parquet(<fixture>)`, as one value.
fn one(what: &str) -> Value {
    let database = Database::new();
    let sql = format!("SELECT {what} FROM read_parquet({})", fixture());
    database.value(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

/// The type the `words` column of the byte array fixture is read as, under a call written out in
/// full.
///
/// The other column of that file is bytes that are not valid UTF-8 under any reading, so it is left
/// out here rather than asserted on. What it should do with `binary_as_string` on it is refuse, and
/// it already refuses without it.
fn word_type(call: &str) -> String {
    let database = Database::new();
    let sql = format!("SELECT words FROM {call}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.types()[0].to_string()
}

/// The plan tree `EXPLAIN` prints for `sql`, which is everything above the first blank line.
fn explained(sql: &str) -> String {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    match result.value_at(0, 1) {
        Value::Varchar(text) => text.lines().take_while(|line| !line.is_empty()).collect(),
        other => panic!("the plan came back as {other:?}"),
    }
}

#[test]
fn counting_the_rows_of_a_file_is_the_number_of_rows_in_it() {
    assert_eq!(one("count(*)"), Value::BigInt(4096));
}

#[test]
fn a_file_read_knows_how_tall_it_is_because_its_footer_says_so() {
    // The binder already has the footer open to read the schema out of it and the row count is in
    // the same few kilobytes, so this is free. It matters out of all proportion to its size: it is
    // the only real number a query over a file starts from, and without it every node above the
    // scan is unknown and the selectivity constants never get anything to multiply.
    let text = explained(&format!("EXPLAIN SELECT a FROM read_parquet({})", fixture()));
    assert!(text.contains("[4096 rows exact from row count]"), "{text}");
}

#[test]
fn a_file_written_where_a_table_goes_knows_how_tall_it_is_too() {
    // The replacement scan is the spelling ClickBench uses and it takes a different road through
    // the binder, so it is measured separately rather than assumed to follow.
    let text = explained(&format!("EXPLAIN SELECT a FROM {}", fixture()));
    assert!(text.contains("[4096 rows exact from row count]"), "{text}");
}

#[test]
fn a_filter_over_a_file_is_a_guess_that_says_it_is_one() {
    // A fifth of a counted number rather than an unknown over an unknown. The value is the
    // constant's and is wrong for this predicate, and the point is that it now exists and admits
    // where it came from.
    let text = explained(&format!("EXPLAIN SELECT a FROM {} WHERE a < 10", fixture()));
    assert!(text.contains("[~819 rows estimated from default]"), "{text}");
}

#[test]
fn file_scan_metrics_count_the_compressed_column_bytes_read() {
    let database = Database::new();
    let sql = format!("SELECT sum(a) FROM read_parquet({})", fixture());
    let result = database.query(&sql).expect("runs");
    let metrics = result.metrics().expect("a query that ran has metrics");
    let scan = metrics
        .operators
        .iter()
        .find(|operator| operator.kind == "FileScan")
        .expect("the query contains a file scan");
    assert!(scan.bytes_read > 0, "the scan reports the Parquet column bytes it read");
    assert_eq!(metrics.resource.bytes_read, scan.bytes_read);
}

#[test]
fn a_file_scan_says_which_stage_of_the_read_its_time_went_to() {
    let database = Database::new();
    let sql = format!("SELECT sum(a) FROM read_parquet({})", fixture());
    let result = database.query(&sql).expect("runs");
    let metrics = result.metrics().expect("a query that ran has metrics");
    let scan = metrics
        .operators
        .iter()
        .find(|operator| operator.kind == "FileScan")
        .expect("the query contains a file scan");
    assert!(!scan.stages.is_empty(), "a scan that read a file spent time in a stage of reading");
    assert!(scan.stages.bytes(Stage::Read) > 0, "the read stage moved the bytes off the file");
    assert!(scan.stages.bytes(Stage::Decode) > 0, "the decode stage turned them into values");
    assert!(
        scan.stages.total() <= scan.wall_ns,
        "the stages are inside the operator rather than beside it: {} against {}",
        scan.stages.total(),
        scan.wall_ns
    );
    // Only the scan reads. Other operators do report stages, because an aggregate says which of its
    // phases took its time on the same clock, but none of them is allowed to be carrying a stage of
    // a read it did not do.
    let reading =
        [Stage::Read, Stage::Decompress, Stage::Decode, Stage::Dictionary, Stage::Assemble];
    for operator in &metrics.operators {
        if operator.kind == "FileScan" {
            continue;
        }
        for stage in reading {
            assert_eq!(
                operator.stages.nanos(stage),
                0,
                "{} reported the {} stage of a read it did not do",
                operator.kind,
                stage.name()
            );
        }
    }
}

#[test]
fn every_column_of_the_file_is_selectable_by_the_name_the_file_gives_it() {
    let database = Database::new();
    let sql = format!("SELECT a, b, s, d, flag, day, t FROM read_parquet({})", fixture());
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 4096);
    assert_eq!(result.width(), 7);
    assert_eq!(result.names(), ["a", "b", "s", "d", "flag", "day", "t"]);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["INTEGER", "BIGINT", "VARCHAR", "DOUBLE", "BOOLEAN", "DATE", "TIMESTAMP"]);
}

#[test]
fn the_first_row_is_the_one_duckdb_reads_from_the_same_bytes() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_parquet({}) LIMIT 1", fixture());
    let result = database.query(&sql).expect("runs");
    let row: Vec<Value> = (0..result.width()).map(|at| result.value_at(0, at)).collect();
    assert_eq!(
        row,
        vec![
            Value::Integer(0),
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
fn the_nulls_of_a_dictionary_column_are_the_nulls_duckdb_counts() {
    // 586 of the 4096 rows, which is every seventh, and this is the assertion that was wrong
    // before `nulls_of` read a dictionary's own validity as well as its values'. A file is the
    // first thing that builds that vector, so nothing here could have failed until now.
    assert_eq!(one("count(s)"), Value::BigInt(3510));
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE s IS NULL", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(586));
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE s IS NOT NULL", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(3510));
}

#[test]
fn a_where_clause_over_a_file_filters_the_rows_it_read() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE flag", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(2048));
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE a < 10", fixture());
    // 97 divides 4096 into 42 whole turns and a remainder of 22, so the values under ten come round
    // 42 times each and the first ten of the remainder add one more apiece.
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(430));
}

#[test]
fn a_where_clause_no_row_group_can_satisfy_reads_none_of_the_file() {
    // The integer column runs 0 to 96 and the footer says so per row group, so a filter asking for
    // values above a thousand is answered out of the footer. The answer has to be right and the
    // bytes read have to be zero: a scan that read the pages and then filtered them would pass the
    // first half of this and fail the second.
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE a > 1000", fixture());
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::BigInt(0));
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert_eq!(metrics.resource.bytes_read, 0, "no page of a ruled out row group was read");
}

#[test]
fn a_where_clause_the_row_groups_can_satisfy_still_reads_them() {
    // The other half. Bounds that overlap the column rule nothing out, and the rows still come back
    // through the filter as they always did.
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE a > 90", fixture());
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::BigInt(252));
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert!(metrics.resource.bytes_read > 0, "the groups that could match were read");
}

#[test]
fn an_aggregate_over_a_dictionary_column_groups_by_its_values() {
    let database = Database::new();
    let sql = format!(
        "SELECT s, count(*) FROM read_parquet({}) WHERE s IS NOT NULL GROUP BY s ORDER BY s",
        fixture()
    );
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 5, "tag0 to tag4");
    assert_eq!(result.value_at(0, 0), Value::Varchar("tag0".into()));
    let total: i64 = (0..5)
        .map(|row| match result.value_at(row, 1) {
            Value::BigInt(count) => count,
            other => panic!("count(*) gave {other}"),
        })
        .sum();
    assert_eq!(total, 3510);
}

#[test]
fn the_sums_are_the_sums_duckdb_reports_for_the_same_file() {
    assert_eq!(one("sum(a)"), Value::HugeInt(195_783));
    assert_eq!(one("sum(b)"), Value::HugeInt(2_002_560_000));
    assert_eq!(one("min(day)"), Value::Date(0));
    assert_eq!(one("max(day)"), Value::Date(999));
}

#[test]
fn an_alias_renames_the_call_and_not_its_columns() {
    let database = Database::new();
    let sql = format!("SELECT p.a FROM read_parquet({}) AS p LIMIT 1", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::Integer(0));
}

#[test]
fn parquet_scan_reads_the_same_file_under_duckdbs_other_name_for_the_function() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM parquet_scan({})", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(4096));
}

#[test]
fn a_file_that_is_not_there_fails_at_bind_time_with_duckdbs_message() {
    let database = Database::new();
    let error =
        database.query("SELECT * FROM read_parquet('/nowhere/at/all.parquet')").unwrap_err();
    assert_eq!(
        error.message(),
        "No files found that match the pattern \"/nowhere/at/all.parquet\""
    );
}

#[test]
fn a_path_that_is_not_a_string_does_not_become_one() {
    let database = Database::new();
    let error = database.query("SELECT * FROM read_parquet(3)").unwrap_err();
    assert!(error.message().contains("read_parquet(INTEGER)"), "{error}");
}

#[test]
fn a_file_name_where_a_table_name_goes_reads_the_file() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM {}", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(4096));
}

#[test]
fn a_double_quoted_file_name_is_the_same_file_as_a_single_quoted_one() {
    let database = Database::new();
    let quoted = fixture().replace('\'', "\"");
    let sql = format!("SELECT count(*) FROM {quoted}");
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(4096));
}

#[test]
fn the_columns_of_a_replaced_file_answer_to_the_stem_of_its_name() {
    // Not to the path, which has a slash and a dot in it and could not be written as a name.
    let database = Database::new();
    let sql = format!("SELECT mixed.a FROM {} LIMIT 1", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::Integer(0));
}

#[test]
fn an_alias_on_a_replaced_file_is_what_the_columns_answer_to_instead() {
    let database = Database::new();
    let sql = format!("SELECT p.a FROM {} AS p LIMIT 1", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::Integer(0));
}

#[test]
fn a_table_of_that_name_is_read_before_a_file_of_that_name() {
    let database = Database::new();
    let name = fixture().replace('\'', "\"");
    database.execute(&format!("CREATE TABLE {name} (x INTEGER)")).expect("creates");
    let sql = format!("SELECT count(*) FROM {}", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(0));
}

#[test]
fn a_name_that_is_not_a_parquet_file_is_still_a_table_that_does_not_exist() {
    let database = Database::new();
    let error = database.query("SELECT * FROM 'notes.txt'").unwrap_err();
    assert_eq!(error.message(), "Table with name notes.txt does not exist!");
}

#[test]
fn a_parquet_file_that_is_not_there_is_the_file_error_and_not_the_table_one() {
    let database = Database::new();
    let error = database.query("SELECT * FROM '/nowhere/at/all.parquet'").unwrap_err();
    assert_eq!(
        error.message(),
        "No files found that match the pattern \"/nowhere/at/all.parquet\""
    );
}

#[test]
fn a_query_that_names_two_columns_plans_a_scan_of_those_two() {
    // The other five are in the file and the plan does not mention them, so the reader is never
    // asked for them. On ClickBench this is three columns out of 105 rather than two out of seven.
    let database = Database::new();
    let sql = format!("SELECT a, s FROM {} WHERE a < 10", fixture());
    let plan = database.plan(&sql).expect("binds");
    assert!(plan.contains("[a::INTEGER, s::VARCHAR]"), "{plan}");
    for dropped in ["b::BIGINT", "d::DOUBLE", "flag::BOOLEAN", "day::DATE", "t::TIMESTAMP"] {
        assert!(!plan.contains(dropped), "{dropped} survived in {plan}");
    }
}

#[test]
fn counting_the_rows_of_a_file_plans_a_scan_of_no_columns_at_all() {
    // Which makes it a read of the footer. The row count is in there and no column chunk has to be
    // touched to add it up.
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM {}", fixture());
    let plan = database.plan(&sql).expect("binds");
    assert!(plan.contains("#0 []"), "{plan}");
}

#[test]
fn a_file_can_be_loaded_into_a_table_once_and_queried_many_times() {
    // Which is the point of `CREATE TABLE AS` in this milestone. A suite that runs forty three
    // queries over one file should decode it once, and the rows a table hands back are the rows
    // the file had.
    let database = Database::new();
    let sql = format!("CREATE TABLE loaded AS SELECT a, s, day FROM {}", fixture());
    database.execute(&sql).expect("loads");
    let result = database.query("SELECT * FROM loaded").expect("runs");
    assert_eq!(result.len(), 4096);
    assert_eq!(result.names(), ["a", "s", "day"]);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["INTEGER", "VARCHAR", "DATE"]);
    assert_eq!(database.value("SELECT sum(a) FROM loaded").expect("runs"), Value::HugeInt(195_783));
    assert_eq!(database.value("SELECT count(s) FROM loaded").expect("runs"), Value::BigInt(3510));
    assert_eq!(database.value("SELECT max(day) FROM loaded").expect("runs"), Value::Date(999));
}

#[test]
fn loading_a_file_into_a_table_reads_the_columns_it_is_loading_and_no_others() {
    // The pruning runs over the query under a `CREATE TABLE AS` as well, and this is the shape
    // where it matters most: loading three columns of `hits` should not decode the other hundred
    // and two on the way past.
    let database = Database::new();
    let sql = format!("SELECT a, s FROM {}", fixture());
    let plan = database.plan(&sql).expect("binds");
    assert!(plan.contains("[a::INTEGER, s::VARCHAR]"), "{plan}");
    let database = Database::new();
    let loading = format!("CREATE TABLE loaded AS {sql}");
    database.execute(&loading).expect("loads");
    assert_eq!(database.value("SELECT count(*) FROM loaded").expect("runs"), Value::BigInt(4096));
    assert_eq!(database.query("SELECT * FROM loaded").expect("runs").width(), 2);
}

#[test]
fn a_table_loaded_from_a_file_can_be_appended_to_from_the_same_file() {
    let database = Database::new();
    database
        .execute(&format!("CREATE TABLE loaded AS SELECT a FROM {}", fixture()))
        .expect("loads");
    database.execute(&format!("INSERT INTO loaded SELECT a FROM {}", fixture())).expect("appends");
    assert_eq!(database.value("SELECT count(*) FROM loaded").expect("runs"), Value::BigInt(8192));
    assert_eq!(
        database.value("SELECT sum(a) FROM loaded").expect("runs"),
        Value::HugeInt(2 * 195_783)
    );
}

#[test]
fn an_unannotated_byte_array_column_is_a_blob_until_the_call_says_it_is_not() {
    // Which is what duckdb reads the same two columns as, and it is the whole difference between
    // the clickbench file loading and failing. Twenty eight of its columns look like this.
    assert_eq!(word_type(&format!("read_parquet({})", bytes())), "BLOB");
    assert_eq!(word_type(&format!("read_parquet({}, binary_as_string=True)", bytes())), "VARCHAR");
}

#[test]
fn a_column_read_as_text_holds_the_text_duckdb_reads_out_of_it() {
    let database = Database::new();
    let call = format!("read_parquet({}, binary_as_string=True)", bytes());
    let sql =
        format!("SELECT count(words), sum(length(words)), min(words), max(words) FROM {call}");
    let result = database.query(&sql).expect("runs");
    let row: Vec<Value> = (0..result.width()).map(|at| result.value_at(0, at)).collect();
    assert_eq!(
        row,
        vec![
            Value::BigInt(1861),
            Value::HugeInt(16749),
            Value::Varchar("byte_0000".into()),
            Value::Varchar("byte_1023".into()),
        ]
    );
}

#[test]
fn a_named_parameter_can_be_written_any_of_the_three_ways_the_binary_takes_it() {
    // `:=` and `=>` are in the grammar and `=` is not, and the clickbench entry writes the one
    // that is not, so all three have to arrive at the same place.
    for spelling in
        ["binary_as_string := True", "binary_as_string => True", "binary_as_string = True"]
    {
        let call = format!("read_parquet({}, {spelling})", bytes());
        assert_eq!(word_type(&call), "VARCHAR", "{spelling}");
    }
    // And the default is off, so writing it false is the same as not writing it.
    assert_eq!(word_type(&format!("read_parquet({}, binary_as_string=False)", bytes())), "BLOB");
}

#[test]
fn a_named_parameter_the_function_does_not_have_is_the_binders_complaint() {
    let database = Database::new();
    let sql = format!("SELECT 1 FROM read_parquet({}, nonesuch=True)", bytes());
    let error = database.query(&sql).unwrap_err();
    assert!(
        error
            .message()
            .starts_with("Invalid named parameter \"nonesuch\" for function read_parquet"),
        "{error}"
    );
    // The list of what the function does take is on the end of the same message, the way the
    // binary prints it, so a reader of the error is told what to write instead.
    assert!(error.message().contains("binary_as_string BOOLEAN"), "{error}");
}

#[test]
fn a_named_parameter_cannot_be_given_null() {
    let database = Database::new();
    let sql = format!("SELECT 1 FROM read_parquet({}, binary_as_string=NULL)", bytes());
    let error = database.query(&sql).unwrap_err();
    assert_eq!(error.message(), "Cannot use NULL as argument to \"binary_as_string\"");
}

#[test]
fn a_column_the_file_does_not_have_is_the_error_a_missing_column_always_is() {
    let database = Database::new();
    let sql = format!("SELECT nosuch FROM read_parquet({})", fixture());
    let error = database.query(&sql).unwrap_err();
    assert!(error.message().contains("nosuch"), "{error}");
}

#[test]
fn file_row_number_is_the_rows_ordinal_inside_its_own_file() {
    // DuckDB v2.0.0-dev84237 over the same fixture answers 4095 and 4096 to the first query and
    // 5, 102, 199 to the second, which is what is written down here.
    let database = Database::new();
    let call = format!("read_parquet({}, file_row_number=True)", fixture());
    let sql = format!("SELECT max(file_row_number), count(*) FROM {call}");
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::BigInt(4095));
    assert_eq!(result.value_at(0, 1), Value::BigInt(4096));
    assert_eq!(result.types()[0].to_string(), "BIGINT");

    let sql = format!("SELECT file_row_number FROM {call} WHERE a = 5 LIMIT 3");
    let result = database.query(&sql).expect("runs");
    let got: Vec<Value> = (0..result.len()).map(|at| result.value_at(at, 0)).collect();
    assert_eq!(got, [Value::BigInt(5), Value::BigInt(102), Value::BigInt(199)]);
}

#[test]
fn the_counted_column_is_there_only_when_the_call_asks_for_it() {
    let database = Database::new();
    let sql = format!("SELECT file_row_number FROM read_parquet({})", fixture());
    let error = database.query(&sql).unwrap_err();
    assert!(error.message().contains("file_row_number"), "{error}");
    let sql =
        format!("SELECT file_row_number FROM read_parquet({}, file_row_number=False)", fixture());
    let error = database.query(&sql).unwrap_err();
    assert!(error.message().contains("file_row_number"), "{error}");
}

#[test]
fn counting_a_column_that_is_not_in_the_file_reads_none_of_the_file() {
    // The pruning case worth pinning. Nothing names a column of the file, so the scan opens it for
    // its row count and reads no column data, and the counted column is still right.
    let database = Database::new();
    let call = format!("read_parquet({}, file_row_number=True)", fixture());
    let sql = format!("SELECT sum(file_row_number) FROM {call}");
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.value_at(0, 0), Value::HugeInt(4095 * 4096 / 2));
    let metrics = result.metrics().expect("a query that ran has metrics");
    assert_eq!(metrics.resource.bytes_read, 0, "no column of the file was read");
}

#[test]
fn the_count_starts_again_at_zero_in_every_file_of_a_glob() {
    // Which is what the name says and what DuckDB does. Three rows in the first part and four in
    // the second, and the same seven values come out of the binary over the same two files.
    let database = Database::new();
    let glob =
        format!("'{}/../rudb-parquet/testdata/parts/p*.parquet'", env!("CARGO_MANIFEST_DIR"));
    let sql = format!("SELECT file_row_number FROM read_parquet({glob}, file_row_number=True)");
    let result = database.query(&sql).expect("runs");
    let got: Vec<i64> = (0..result.len())
        .map(|at| match result.value_at(at, 0) {
            Value::BigInt(number) => number,
            other => panic!("a BIGINT column produced {other:?}"),
        })
        .collect();
    assert_eq!(got, [0, 1, 2, 0, 1, 2, 3]);
}
