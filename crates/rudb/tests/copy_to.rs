//! `COPY ... TO` a CSV or a JSON file, checked byte for byte against what the pinned DuckDB wrote for the
//! same statements, and a Parquet file, checked by reading it back.

use std::path::PathBuf;

use rudb::Database;

const TABLE: &str = "CREATE TABLE t AS SELECT * FROM (VALUES (1, 'a,b', 1.5::DOUBLE, \
    DATE '2026-01-02', NULL::VARCHAR, TIMESTAMP '2026-01-02 03:04:05.5', 'say \"hi\"', [1,2], \
    {'k': 1}, true, 12.30::DECIMAL(5,2), '', 'line\ntwo'), (2, 'plain', -0.0, NULL, 'x', NULL, \
    'q''s', [], NULL, false, NULL, ' pad ', 'end')) v(i, s, d, dt, n, ts, q, l, st, b, dec, e, nl)";

fn file(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rudb-copy-to-{tag}-{}.csv", std::process::id()))
}

fn written(db: &Database, tag: &str, statement: &str) -> String {
    let path = file(tag);
    let sql = statement.replace("FILE", &path.display().to_string());
    let result = db.execute(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let text = std::fs::read_to_string(&path).expect("the file is there");
    assert!(result.changes().is_some(), "{sql} answers a count");
    let _ = std::fs::remove_file(&path);
    text
}

fn table() -> Database {
    let db = Database::new();
    db.execute(TABLE).expect("creates");
    db
}

#[test]
fn every_type_is_written_as_its_text_and_quoted_only_when_it_has_to_be() {
    let db = table();
    assert_eq!(
        written(&db, "all", "COPY t TO 'FILE'"),
        "i,s,d,dt,n,ts,q,l,st,b,dec,e,nl\n\
         1,\"a,b\",1.5,2026-01-02,,2026-01-02 03:04:05.5,\"say \"\"hi\"\"\",\"[1, 2]\",{'k': 1},true,12.30,\"\",\"line\ntwo\"\n\
         2,plain,0.0,,x,,q's,[],,false,, pad ,end\n"
    );
}

#[test]
fn the_options_change_the_delimiter_the_quote_the_null_and_the_header() {
    let db = table();
    assert_eq!(
        written(
            &db,
            "options",
            "COPY t TO 'FILE' (HEADER false, DELIMITER '|', NULL 'NA', QUOTE '''', FORCE_QUOTE (i))"
        ),
        "'1'|a,b|1.5|2026-01-02|NA|2026-01-02 03:04:05.5|say \"hi\"|[1, 2]|'{\"'k\"': 1}'|true|12.30||'line\ntwo'\n\
         '2'|plain|0.0|NA|x|NA|'q\"'s'|[]|NA|false|NA| pad |end\n"
    );
    assert_eq!(
        written(&db, "tab", "COPY (SELECT i, s FROM t) TO 'FILE' (SEP '\\t', HEADER)"),
        "i\ts\n1\ta,b\n2\tplain\n"
    );
    assert_eq!(
        written(&db, "bare", "COPY (SELECT i FROM t) TO 'FILE' WITH (FORMAT csv, HEADER 0)"),
        "1\n2\n"
    );
}

#[test]
fn a_table_with_a_column_list_is_the_query_over_those_columns() {
    let db = table();
    let want = "i,s\n1,\"a,b\"\n2,plain\n";
    assert_eq!(written(&db, "query", "COPY (SELECT i, s FROM t) TO 'FILE' (FORMAT csv)"), want);
    assert_eq!(written(&db, "columns", "COPY t (i, s) TO 'FILE'"), want);
}

#[test]
fn numbers_times_and_blobs_are_written_the_way_the_pin_casts_them() {
    let db = Database::new();
    db.execute("SET TimeZone = 'Europe/Berlin'").expect("sets the zone");
    assert_eq!(
        written(
            &db,
            "types",
            "COPY (SELECT 1.0e20::DOUBLE AS x, 1e-7::DOUBLE AS y, 0.1::FLOAT AS z, 'inf'::DOUBLE AS w, \
             INTERVAL 3 DAY AS iv, TIME '01:02:03' AS tm, '\\x00\\xFF'::BLOB AS bl, 'aa'::BLOB AS b2, \
             123456789012::HUGEINT AS h, TIMESTAMPTZ '2026-01-02 03:04:05+00' AS tz) TO 'FILE'"
        ),
        "x,y,z,w,iv,tm,bl,b2,h,tz\n\
         1e+20,1e-07,0.1,inf,3 days,01:02:03,\\x00\\xFF,aa,123456789012,2026-01-02 04:04:05+01\n"
    );
}

#[test]
fn what_this_does_not_write_is_refused_by_name() {
    let db = Database::new();
    for (sql, message) in [
        ("COPY (SELECT 1 AS x) TO 'o.csv' (BOGUS 1)", "Unrecognized option \"bogus\" for csv"),
        (
            "COPY (SELECT 1 AS x) TO 'o.parquet' (COMPRESSION zstd)",
            "zstd codec is not supported yet",
        ),
        (
            "COPY (SELECT 1 AS x) TO 'o.parquet' (BOGUS 1)",
            "Unrecognized option \"bogus\" for parquet",
        ),
        ("COPY (SELECT [1] AS x) TO 'o.parquet'", "type INTEGER[] is not supported yet"),
    ] {
        let error = db.execute(sql).expect_err(sql).to_string();
        assert!(error.starts_with("Not implemented Error"), "{sql} gave {error}");
        assert!(error.contains(message), "{sql} gave {error}");
    }
}

#[test]
fn json_is_one_object_a_line_or_one_array() {
    let db = table();
    let rows = "{\"i\":1,\"s\":\"a,b\",\"d\":1.5,\"dt\":\"2026-01-02\",\"n\":null,\
        \"ts\":\"2026-01-02 03:04:05.5\",\"q\":\"say \\\"hi\\\"\",\"l\":[1,2],\"st\":{\"k\":1},\
        \"b\":true,\"dec\":12.3,\"e\":\"\",\"nl\":\"line\\ntwo\"}";
    let second = "{\"i\":2,\"s\":\"plain\",\"d\":0.0,\"dt\":null,\"n\":\"x\",\"ts\":null,\
        \"q\":\"q's\",\"l\":[],\"st\":null,\"b\":false,\"dec\":null,\"e\":\" pad \",\"nl\":\"end\"}";
    assert_eq!(
        written(&db, "json", "COPY t TO 'FILE' (FORMAT json)"),
        format!("{rows}\n{second}\n")
    );
    assert_eq!(
        written(&db, "array", "COPY t TO 'FILE' (FORMAT json, ARRAY true)"),
        format!("[\n\t{rows},\n\t{second}\n]\n")
    );
    assert_eq!(
        written(&db, "none", "COPY (SELECT 1 AS a WHERE false) TO 'FILE' (FORMAT json, ARRAY)"),
        "[\n\t\n]\n"
    );
    assert_eq!(
        written(&db, "empty", "COPY (SELECT 1 AS a WHERE false) TO 'FILE' (FORMAT json)"),
        ""
    );
}

#[test]
fn json_doubles_nesting_and_escapes_are_the_pins() {
    let db = table();
    assert_eq!(
        written(
            &db,
            "doubles",
            "COPY (SELECT unnest([1e21, 9.99e20, 1.5e-7, 1e-6, 0.0000015, 1.25e300, 5e-324, \
             -2.5e-8, 100.0, 'inf'::DOUBLE, '-inf'::DOUBLE, 'nan'::DOUBLE]::DOUBLE[]) AS x) TO 'FILE' (FORMAT json)"
        ),
        "{\"x\":1e21}\n{\"x\":999000000000000000000.0}\n{\"x\":1.5e-7}\n{\"x\":0.000001}\n\
         {\"x\":0.0000015}\n{\"x\":1.25e300}\n{\"x\":5e-324}\n{\"x\":-2.5e-8}\n{\"x\":100.0}\n\
         {\"x\":Infinity}\n{\"x\":-Infinity}\n{\"x\":NaN}\n"
    );
    db.execute("SET TimeZone = 'Europe/Paris'").expect("sets");
    assert_eq!(
        written(
            &db,
            "nested",
            "COPY (SELECT 0.1::FLOAT AS f, TIMESTAMPTZ '2026-01-02 03:04:05+00' AS tz, \
             [TIMESTAMPTZ '2026-01-02 03:04:05+00'] AS ltz, MAP {'k': [1.5::DECIMAL(3,1)]} AS m, \
             chr(1) || chr(8) || chr(12) || '/' || chr(127) AS c, '\\x00'::BLOB AS bl, \
             INTERVAL 3 DAY AS iv, 170141183460469231731687303715884105727::HUGEINT AS h, \
             [NULL, {'a': 'x'}] AS ls) TO 'FILE' (FORMAT json)"
        ),
        "{\"f\":0.10000000149011612,\"tz\":\"2026-01-02 04:04:05+01\",\
         \"ltz\":[\"2026-01-02 04:04:05+01\"],\"m\":{\"k\":[1.5]},\"c\":\"\\u0001\\b\\f/\u{7f}\",\
         \"bl\":\"\\\\x00\",\"iv\":\"3 days\",\"h\":170141183460469231731687303715884105727,\
         \"ls\":[null,{\"a\":\"x\"}]}\n"
    );
}

#[test]
fn json_options_are_the_pins() {
    let db = Database::new();
    for (sql, message) in [
        (
            "COPY (SELECT 1 AS x) TO 'o.json' (HEADER true)",
            "Binder Error: Unknown option for COPY ... TO ... (FORMAT JSON): \"header\".",
        ),
        (
            "COPY (SELECT 1 AS x) TO 'o.json' (ARRAY maybe)",
            "Invalid Input Error: Failed to cast value: Could not convert string 'maybe' to BOOL",
        ),
        (
            "COPY (SELECT 1 AS x) TO 'o.json' (FORMAT xml)",
            "Catalog Error: Copy Function with name xml does not exist!",
        ),
    ] {
        let error = db.execute(sql).expect_err(sql).to_string();
        assert!(error.starts_with(message), "{sql} gave {error}");
    }
}

fn values(db: &Database, sql: &str) -> Vec<Vec<rudb_common::Value>> {
    let result = db.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

#[test]
fn a_parquet_file_reads_back_as_what_was_written() {
    let db = Database::new();
    db.execute(
        "CREATE TABLE p (a TINYINT, b SMALLINT, c INTEGER, d BIGINT, e UTINYINT, f USMALLINT, \
         g UINTEGER, h UBIGINT, i FLOAT, j DOUBLE, k VARCHAR, l BLOB, m BOOLEAN, n DATE, o TIME, \
         q TIMESTAMP, r TIMESTAMPTZ, s DECIMAL(4,2), t DECIMAL(15,3), u DECIMAL(30,1))",
    )
    .expect("creates");
    db.execute(
        "INSERT INTO p VALUES \
         (1, 2, 3, 4, 5, 6, 7, 8, 1.5, 2.5, 'text', 'blob\\x41', true, DATE '2026-01-02', \
          TIME '01:02:03.5', TIMESTAMP '2026-01-02 03:04:05.25', \
          TIMESTAMPTZ '2026-01-02 03:04:05+00', 12.34, 1234567.891, \
          -123456789012345678901.5), \
         (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
          NULL, NULL, NULL, NULL, NULL, NULL), \
         (-1, -2, -3, -4, 250, 65000, 4000000000, 18000000000000000000, -1.5, -2.5, '', '', false, \
          DATE '1970-01-01', TIME '00:00:00', TIMESTAMP '1969-12-31 23:59:59', \
          TIMESTAMPTZ '1969-12-31 23:59:59+00', -0.01, 0, 99999999999999999999999999999.9)",
    )
    .expect("inserts");
    // The blob is text, because a blob that is not reads back only once the reader has a byte
    // column to put it in.
    db.execute("SET TimeZone = 'UTC'").expect("sets");
    for codec in ["snappy", "uncompressed"] {
        let path = std::env::temp_dir()
            .join(format!("rudb-copy-to-{codec}-{}.parquet", std::process::id()));
        let sql = format!("COPY p TO '{}' (COMPRESSION {codec})", path.display());
        let result = db.execute(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        assert!(result.changes().is_some(), "{sql} answers a count");
        let back = format!("read_parquet('{}')", path.display());
        assert_eq!(
            values(&db, &format!("SELECT * FROM {back} ORDER BY c NULLS LAST")),
            values(&db, "SELECT * FROM p ORDER BY c NULLS LAST"),
            "{codec}"
        );
        let types = |from: &str| values(&db, &format!("SELECT column_type FROM (DESCRIBE {from})"));
        assert_eq!(types(&format!("SELECT * FROM {back}")), types("SELECT * FROM p"), "{codec}");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn a_parquet_file_of_many_row_groups_reads_back_whole() {
    let db = Database::new();
    let path =
        std::env::temp_dir().join(format!("rudb-copy-to-groups-{}.parquet", std::process::id()));
    let sql = format!(
        "COPY (SELECT range AS id, 'r' || range AS tag, CASE WHEN range % 7 = 0 THEN NULL \
         ELSE range * 2 END AS twice FROM range(20000)) TO '{}' (ROW_GROUP_SIZE 4096)",
        path.display()
    );
    db.execute(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let back = format!("read_parquet('{}')", path.display());
    let got = values(&db, &format!("SELECT count(*), sum(id), count(twice), max(tag) FROM {back}"));
    let expected = values(
        &db,
        "SELECT count(*), sum(range), count(*) FILTER (WHERE range % 7 <> 0), max('r' || range) \
         FROM range(20000)",
    );
    assert_eq!(got, expected);
    let got = values(&db, &format!("SELECT tag, twice FROM {back} WHERE id = 12345"));
    assert_eq!(
        got,
        vec![vec![rudb_common::Value::Varchar("r12345".into()), rudb_common::Value::BigInt(24690)]]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_exported_state_is_written_as_its_layout_and_reads_back_into_a_state() {
    let db = Database::new();
    let path =
        std::env::temp_dir().join(format!("rudb-copy-to-state-{}.parquet", std::process::id()));
    let sql = format!(
        "COPY (SELECT count(*) EXPORT_STATE AS c, sum(42) EXPORT_STATE AS state) TO '{}'",
        path.display()
    );
    db.execute(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let back = format!("read_parquet('{}')", path.display());
    let types = values(&db, &format!("SELECT column_type FROM (DESCRIBE SELECT * FROM {back})"));
    let text = |v: &str| rudb_common::Value::Varchar(v.into());
    assert_eq!(types, vec![vec![text("BIGINT")], vec![text("DOUBLE")]]);
    let got = values(
        &db,
        &format!(
            "SELECT to_aggregate_state(state, 'sum', ['INTEGER'])::VARCHAR, \
             finalize(to_aggregate_state(state, 'sum', ['INTEGER']))::VARCHAR FROM {back}"
        ),
    );
    assert_eq!(got, vec![vec![text("42"), text("42")]]);
    let _ = std::fs::remove_file(&path);
}
