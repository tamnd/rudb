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
fn an_empty_quote_writes_every_value_bare_and_escapes_nothing() {
    let db = table();
    let want = "i,s,d,dt,n,ts,q,l,st,b,dec,e,nl\n\
                1,a,b,1.5,2026-01-02,,2026-01-02 03:04:05.5,say \"hi\",[1, 2],{'k': 1},true,12.30,,line\ntwo\n\
                2,plain,0.0,,x,,q's,[],,false,, pad ,end\n";
    assert_eq!(written(&db, "bare-quote", "COPY t TO 'FILE' (QUOTE '')"), want);
    assert_eq!(
        written(&db, "bare-forced", "COPY t TO 'FILE' (QUOTE '', FORCE_QUOTE (i), ESCAPE '\\')"),
        want
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
fn a_json_date_or_timestamp_is_written_through_the_format_given() {
    let db = Database::new();
    db.execute("SET TimeZone = 'Europe/Berlin'").expect("sets");
    assert_eq!(
        written(
            &db,
            "formats",
            "COPY (SELECT DATE '1996-03-27' AS d, TIMESTAMP '1996-03-27 07:42:33' AS t, \
             [DATE '2000-01-02'] AS l, {'x': TIMESTAMP '2001-02-03 04:05:06'} AS s, \
             TIMESTAMPTZ '2001-02-03 04:05:06+00' AS tz, TIME '01:02:03' AS tm, NULL::DATE AS n, \
             '2001-02-03 04:05:06.5'::TIMESTAMP_MS AS ms, '2001-02-03 04:05:06'::TIMESTAMP_S AS sec, \
             '2001-02-03 04:05:06.123456789'::TIMESTAMP_NS AS ns, 'infinity'::DATE AS inf, \
             MAP {DATE '2000-01-02': 1} AS m) TO 'FILE' \
             (FORMAT json, dateformat '%d/%m/%Y', timestampformat '%Y %H %n')"
        ),
        "{\"d\":\"27/03/1996\",\"t\":\"1996 07 000000000\",\"l\":[\"02/01/2000\"],\
         \"s\":{\"x\":\"2001 04 000000000\"},\"tz\":\"2001 05 000000000\",\"tm\":\"01:02:03\",\
         \"n\":null,\"ms\":\"2001 04 500000000\",\"sec\":\"2001 04 000000000\",\
         \"ns\":\"2001 04 123456789\",\"inf\":\"infinity\",\"m\":{\"02/01/2000\":1}}\n"
    );
    let pair = "SELECT DATE '1996-03-27' AS d, TIMESTAMP '1996-03-27 07:42:33' AS t";
    assert_eq!(
        written(&db, "date", &format!("COPY ({pair}) TO 'FILE' (FORMAT json, dateformat '%d')")),
        "{\"d\":\"27\",\"t\":\"1996-03-27 07:42:33\"}\n"
    );
    assert_eq!(
        written(
            &db,
            "stamp",
            &format!("COPY ({pair}) TO 'FILE' (FORMAT json, timestampformat '%H', array true)")
        ),
        "[\n\t{\"d\":\"1996-03-27\",\"t\":\"07\"}\n]\n"
    );
    for (option, message) in [
        (
            "dateformat",
            "Binder Error: COPY (FORMAT JSON) parameter \"dateformat\" expects a single argument.",
        ),
        (
            "timestampformat NULL",
            "Binder Error: COPY (FORMAT JSON) parameter \"timestampformat\" cannot be NULL.",
        ),
        (
            "dateformat '%Q'",
            "Invalid Input Error: Failed to parse format specifier %Q: Unrecognized format for \
             strftime/strptime: %Q",
        ),
    ] {
        let sql = format!("COPY (SELECT 1 AS x) TO 'o.json' (FORMAT json, {option})");
        let error = db.execute(&sql).expect_err(&sql).to_string();
        assert_eq!(error, message, "{sql}");
    }
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

#[test]
fn an_option_written_as_null_or_as_the_wrong_type_is_refused_in_the_pins_words() {
    let db = Database::new();
    for (format, option, message) in [
        (
            "json",
            "dateformat NULL",
            "Binder Error: COPY (FORMAT JSON) parameter \"dateformat\" cannot be NULL.",
        ),
        (
            "json",
            "array NULL",
            "Binder Error: COPY (FORMAT JSON) parameter \"array\" cannot be NULL.",
        ),
        (
            "json",
            "use_tmp_file NULL",
            "Binder Error: COPY (FORMAT JSON) parameter \"use_tmp_file\" cannot be NULL.",
        ),
        (
            "json",
            "dateformat 1, timestampformat NULL",
            "Binder Error: COPY (FORMAT JSON) parameter \"timestampformat\" cannot be NULL.",
        ),
        (
            "json",
            "nosuch NULL",
            "Binder Error: Unknown option for COPY ... TO ... (FORMAT JSON): nosuch.",
        ),
        (
            "json",
            "header NULL",
            "Binder Error: NULL is not supported as a valid option for COPY option \"header\"",
        ),
        (
            "json",
            "dateformat NULL::VARCHAR",
            "Binder Error: NULL is not supported as a valid option for COPY option \"dateformat\"",
        ),
        (
            "csv",
            "use_tmp_file NULL",
            "Binder Error: NULL is not supported as a valid option for COPY option \"use_tmp_file\"",
        ),
        (
            "csv",
            "delimiter NULL",
            "Binder Error: NULL is not supported as a valid option for COPY option \"delimiter\"",
        ),
        (
            "parquet",
            "use_tmp_file NULL",
            "Binder Error: NULL is not supported as a valid option for COPY option \"use_tmp_file\"",
        ),
        (
            "json",
            "dateformat TRUE",
            "Binder Error: COPY (FORMAT JSON) parameter \"dateformat\" expects a VARCHAR argument, but got BOOLEAN.",
        ),
        (
            "json",
            "timestampformat 1.5",
            "Binder Error: COPY (FORMAT JSON) parameter \"timestampformat\" expects a VARCHAR argument, but got DECIMAL(2,1).",
        ),
        (
            "json",
            "file_extension 42",
            "Binder Error: COPY (FORMAT JSON) parameter \"file_extension\" expects a VARCHAR argument, but got INTEGER.",
        ),
        (
            "json",
            "dateformat [1, 2, 3]",
            "Binder Error: COPY (FORMAT JSON) parameter \"dateformat\" expects a VARCHAR argument, but got INTEGER[].",
        ),
        (
            "json",
            "file_extension {'a': 1}",
            "Binder Error: COPY (FORMAT JSON) parameter \"file_extension\" expects a VARCHAR argument, but got STRUCT(a INTEGER).",
        ),
        (
            "json",
            "filename_pattern FALSE",
            "Invalid Input Error: Copy option \"filename_pattern\" expected an argument of type VARCHAR - the argument \"false\" of type BOOLEAN could not be cast as this type",
        ),
        (
            "json",
            "filename_pattern [1]",
            "Invalid Input Error: Copy option \"filename_pattern\" expected an argument of type VARCHAR - the argument \"[1]\" of type INTEGER[] could not be cast as this type",
        ),
        (
            "json",
            "compression 1",
            "Invalid Input Error: Copy option \"compression\" expected an argument of type VARCHAR - the argument \"1\" of type INTEGER could not be cast as this type",
        ),
        (
            "json",
            "encoding 'utf8'",
            "Invalid Input Error: Option \"encoding\" is not supported for writing - only for reading",
        ),
        (
            "json",
            "nosuch 1",
            "Binder Error: Unknown option for COPY ... TO ... (FORMAT JSON): nosuch.",
        ),
    ] {
        let sql = format!("COPY (SELECT 1 AS a) TO 'o.{format}' (FORMAT {format}, {option})");
        let error = db.execute(&sql).expect_err(&sql).to_string();
        assert_eq!(error, message, "{sql}");
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

/// Every file under `root`, as its path below it and what it holds, in path order.
fn files(root: &std::path::Path) -> String {
    fn walk(directory: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("lists the directory").flatten() {
            if entry.file_type().expect("has a type").is_dir() {
                walk(&entry.path(), out);
            } else {
                out.push(entry.path());
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort();
    let mut out = String::new();
    for path in found {
        let name = path.strip_prefix(root).expect("is below the root").display().to_string();
        let text = std::fs::read_to_string(&path).expect("reads the file");
        out.push_str(&format!("== {name}\n{text}"));
    }
    out
}

#[test]
fn partition_by_writes_a_directory_a_value_the_way_the_pin_lays_them_out() {
    let root = std::env::temp_dir().join(format!("rudb-copy-to-hive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.display().to_string();
    let db = Database::new();
    db.execute(
        "CREATE TABLE p AS SELECT * FROM (VALUES (1, 'x', DATE '2020-01-01'), (2, 'a b', NULL), \
         (3, 'x', DATE '2020-01-01'), (4, '__hive_default_partition__', DATE '2020-01-02')) \
         v(id, k, d)",
    )
    .expect("creates");
    let run = |sql: &str| db.execute(&sql.replace("DIR", &dir));
    let copied = run("COPY p TO 'DIR/a' (FORMAT csv, PARTITION_BY (k, d))").expect("copies");
    assert_eq!(copied.changes(), Some(4));
    assert_eq!(
        files(&root.join("a")),
        "== k=%5F_hive_default_partition__/d=2020-01-02/data_0.csv\nid\n4\n\
         == k=a%20b/d=__HIVE_DEFAULT_PARTITION__/data_0.csv\nid\n2\n\
         == k=x/d=2020-01-01/data_0.csv\nid\n1\n3\n"
    );
    let again = run("COPY p TO 'DIR/a' (FORMAT csv, PARTITION_BY (k, d))").unwrap_err();
    assert_eq!(
        again.to_string(),
        format!(
            "IO Error: Directory \"{dir}/a\" is not empty! Enable OVERWRITE option to overwrite \
             files"
        )
    );
    run("COPY (SELECT 9 AS id, 'x' AS k, DATE '2020-01-01' AS d) TO 'DIR/a' \
         (FORMAT csv, PARTITION_BY (k, d), OVERWRITE_OR_IGNORE)")
    .expect("writes over the one file");
    let read = "SELECT string_agg(id || ':' || k || ':' || coalesce(d::VARCHAR, '-'), ' ' \
                ORDER BY id) FROM read_csv('DIR/a/*/*/*.csv', hive_partitioning = true)";
    let read = db.query(&read.replace("DIR", &dir)).expect("reads back");
    assert_eq!(
        read.value_at(0, 0),
        rudb_common::Value::Varchar(
            "2:a b:- 4:__hive_default_partition__:2020-01-02 9:x:2020-01-01".into()
        )
    );

    // Side by side, the files are numbered by the order of the values.
    run("COPY p TO 'DIR/b' (FORMAT csv, PARTITION_BY k, HIVE_FILE_PATTERN false, \
         FILENAME_PATTERN 'out_{i}', FILE_EXTENSION 'txt', WRITE_PARTITION_COLUMNS)")
    .expect("copies");
    assert_eq!(
        files(&root.join("b")),
        "== out_0.txt\nid,k,d\n4,__hive_default_partition__,2020-01-02\n\
         == out_1.txt\nid,k,d\n2,a b,\n\
         == out_2.txt\nid,k,d\n1,x,2020-01-01\n3,x,2020-01-01\n"
    );
    run("COPY (SELECT * FROM (VALUES (1, 30), (2, 10), (3, 20), (4, 5)) v(id, k)) TO 'DIR/n' \
         (FORMAT csv, PARTITION_BY 'k', HIVE_FILE_PATTERN false)")
    .expect("copies");
    assert_eq!(
        files(&root.join("n")),
        "== data_0.csv\nid\n4\n== data_1.csv\nid\n2\n== data_2.csv\nid\n3\n== data_3.csv\nid\n1\n"
    );

    // OVERWRITE takes away every file that was there.
    run("COPY p TO 'DIR/c' (FORMAT json, PARTITION_BY (d))").expect("copies");
    run("COPY (SELECT 1 AS id, 'q' AS k) TO 'DIR/c' (FORMAT json, PARTITION_BY (k), OVERWRITE)")
        .expect("copies");
    assert_eq!(files(&root.join("c")), "== k=q/data_0.json\n{\"id\":1}\n");

    // APPEND names its files with a UUID, so a second copy adds to the first.
    for _ in 0..2 {
        run("COPY p TO 'DIR/e' (FORMAT parquet, PARTITION_BY (k), APPEND)").expect("appends");
    }
    let count = db
        .query(&format!("SELECT count(*) FROM read_parquet('{dir}/e/*/*.parquet')"))
        .expect("reads back");
    assert_eq!(count.value_at(0, 0), rudb_common::Value::BigInt(8));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_partitioned_copy_refuses_what_the_pin_refuses() {
    let db = Database::new();
    db.execute("CREATE TABLE p AS SELECT 1 AS id, 'x' AS k").expect("creates");
    for (sql, want) in [
        (
            "COPY p TO 'e' (PARTITION_BY (k), APPEND, FILENAME_PATTERN 'f')",
            "Binder Error: APPEND mode requires a {uuid} label in filename_pattern",
        ),
        (
            "COPY p TO 'e.csv' (APPEND, FILENAME_PATTERN 'x')",
            "Binder Error: APPEND mode requires a {uuid} label in filename_pattern",
        ),
        (
            "COPY p TO 'e' (PARTITION_BY (k), APPEND, OVERWRITE)",
            "Binder Error: Can only set one of OVERWRITE_OR_IGNORE, OVERWRITE or APPEND",
        ),
        (
            "COPY p TO 'e' (PARTITION_BY (k), USE_TMP_FILE)",
            "Not implemented Error: Can't combine USE_TMP_FILE and PARTITIONED BY for COPY",
        ),
        (
            "COPY p TO 'e' (PARTITION_BY (k), PER_THREAD_OUTPUT)",
            "Not implemented Error: Can't combine PER_THREAD_OUTPUT and PARTITIONED BY for COPY",
        ),
        (
            "COPY p TO 'e' (PARTITION_BY (zz))",
            "Binder Error: \"partition_by\" expected to find zz, but it was not found in the table",
        ),
        (
            "COPY p TO 'e' (PARTITION_BY (k, K))",
            "Binder Error: \"partition_by\" does now allow duplicate columns (found: K)",
        ),
        (
            "COPY p TO 'e' (PARTITION_BY (k, id))",
            "Not implemented Error: No column to write as all columns are specified as partition \
             columns. WRITE_PARTITION_COLUMNS option can be used to write partition columns.",
        ),
    ] {
        let error = db.execute(sql).expect_err(sql);
        assert_eq!(error.to_string(), want, "{sql}");
    }
}

/// Every file under `root`, as its path below it with how many lines and bytes it holds, the way
/// `wc -lc` counts them, in path order.
fn sizes(root: &std::path::Path) -> Vec<String> {
    let listed = files(root);
    let mut out: Vec<String> = Vec::new();
    for part in listed.split("== ").filter(|part| !part.is_empty()) {
        let (name, text) = part.split_once('\n').expect("a name line");
        out.push(format!("{name} {} {}", text.matches('\n').count(), text.len()));
    }
    out
}

#[test]
fn file_size_bytes_and_row_groups_per_file_split_the_rows_over_numbered_files_as_the_pin_does() {
    let root = std::env::temp_dir().join(format!("rudb-copy-to-rotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.display().to_string();
    let db = Database::new();
    let run = |sql: &str| db.execute(&sql.replace("DIR", &dir));

    // A file is closed between batches of 2048 rows once it has reached the size.
    let copied = run("COPY (FROM range(5000)) TO 'DIR/d' (FORMAT csv, FILE_SIZE_BYTES 1.5e3)")
        .expect("copies");
    assert_eq!(copied.changes(), Some(5000));
    let want = ["data_0.csv 2049 9136", "data_1.csv 2049 10246", "data_2.csv 905 4526"];
    assert_eq!(sizes(&root.join("d")), want);
    run(
        "COPY (FROM range(5000)) TO 'DIR/r' (FORMAT csv, PER_THREAD_OUTPUT, FILE_SIZE_BYTES '1kb')",
    )
    .expect("copies");
    assert_eq!(sizes(&root.join("r")), want);
    run(
        "COPY (FROM range(5000)) TO 'DIR/n.csv' (FILE_SIZE_BYTES '1kb', FILENAME_PATTERN 'x_{i}', \
         FILE_EXTENSION 'txt')",
    )
    .expect("copies");
    assert_eq!(
        sizes(&root.join("n.csv")),
        ["x_0.txt 2049 9136", "x_1.txt 2049 10246", "x_2.txt 905 4526"]
    );

    // PER_THREAD_OUTPUT alone shares the rows out over a file for each thread, numbered from 0.
    run("SET threads = 4").expect("sets");
    run("COPY (FROM range(10000)) TO 'DIR/t' (FORMAT csv, PER_THREAD_OUTPUT)").expect("copies");
    assert!(std::fs::read_dir(root.join("t")).expect("lists").count() > 1);
    let threads = db
        .query(&format!("SELECT count(*), sum(range) FROM read_csv('{dir}/t/data_*.csv')"))
        .expect("reads back");
    assert_eq!(threads.value_at(0, 0), rudb_common::Value::BigInt(10000));
    assert_eq!(threads.value_at(0, 1), rudb_common::Value::HugeInt(49_995_000));

    // OVERWRITE_OR_IGNORE writes over the files of the same name and leaves the rest.
    run("COPY (FROM range(5000)) TO 'DIR/i' (FORMAT csv, ROW_GROUPS_PER_FILE 1)").expect("copies");
    run("COPY (FROM range(100)) TO 'DIR/i' (FORMAT csv, ROW_GROUPS_PER_FILE 1, \
         OVERWRITE_OR_IGNORE)")
    .expect("copies");
    assert_eq!(
        sizes(&root.join("i")),
        ["data_0.csv 101 296", "data_1.csv 2049 10246", "data_2.csv 905 4526"]
    );

    // No rows still write the one file, unless WRITE_EMPTY_FILE is off.
    run("COPY (SELECT 1 AS a WHERE false) TO 'DIR/e' (FORMAT csv, FILE_SIZE_BYTES '1kb')")
        .expect("copies");
    assert_eq!(sizes(&root.join("e")), ["data_0.csv 1 2"]);
    run("COPY (SELECT 1 AS a WHERE false) TO 'DIR/e.csv' (WRITE_EMPTY_FILE false)")
        .expect("copies");
    assert!(!root.join("e.csv").exists());

    // JSON has no header, so a file of no bytes takes a batch however small the size.
    run("COPY (FROM range(10000)) TO 'DIR/j' (FORMAT json, FILE_SIZE_BYTES 1)").expect("copies");
    let lines = sizes(&root.join("j"))
        .iter()
        .map(|file| file.split(' ').nth(1).expect("a count").to_string())
        .collect::<Vec<_>>();
    assert_eq!(lines, ["2048", "2048", "2048", "2048", "1808"]);

    // A Parquet batch is as many batches of 2048 rows as it takes to fill a row group.
    run("COPY (FROM range(10000)) TO 'DIR/p' (FORMAT parquet, ROW_GROUP_SIZE 2000, \
         ROW_GROUPS_PER_FILE 2)")
    .expect("copies");
    let counts = db
        .query(&format!(
            "SELECT count(*) FROM read_parquet('{dir}/p/*.parquet', filename = true) \
             GROUP BY filename ORDER BY filename"
        ))
        .expect("reads back");
    let counts = (0..counts.len()).map(|row| counts.value_at(row, 0)).collect::<Vec<_>>();
    assert_eq!(counts, [4096, 4096, 1808].map(rudb_common::Value::BigInt));

    // A partition rotates in its own directory, or numbered on when the files are side by side.
    let pairs = "SELECT i % 2 AS k, i FROM range(5000) t(i)";
    run(&format!("COPY ({pairs}) TO 'DIR/k' (FORMAT csv, PARTITION_BY k, FILE_SIZE_BYTES '1kb')"))
        .expect("copies");
    assert_eq!(
        sizes(&root.join("k")),
        [
            "k=0/data_0.csv 2049 9687",
            "k=0/data_1.csv 453 2262",
            "k=1/data_0.csv 2049 9687",
            "k=1/data_1.csv 453 2262"
        ]
    );
    run(&format!(
        "COPY ({pairs}) TO 'DIR/f' (FORMAT csv, PARTITION_BY k, HIVE_FILE_PATTERN false, \
         ROW_GROUPS_PER_FILE 1)"
    ))
    .expect("copies");
    assert_eq!(
        sizes(&root.join("f")),
        [
            "data_0.csv 2049 9687",
            "data_1.csv 453 2262",
            "data_2.csv 2049 9687",
            "data_3.csv 453 2262"
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_rotating_copy_refuses_what_the_pin_refuses() {
    let db = Database::new();
    for (sql, want) in [
        (
            "COPY (FROM range(5)) TO 'e.csv' (USE_TMP_FILE false, PER_THREAD_OUTPUT)",
            "Not implemented Error: Can't combine USE_TMP_FILE and PER_THREAD_OUTPUT for COPY",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (USE_TMP_FILE, FILE_SIZE_BYTES '1kb')",
            "Not implemented Error: Can't combine USE_TMP_FILE and \
             FILE_SIZE_BYTES/BATCHES_PER_FILE for COPY",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (OVERWRITE false, APPEND true)",
            "Binder Error: Can only set one of OVERWRITE_OR_IGNORE, OVERWRITE or APPEND",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (PER_THREAD_OUTPUT, PARTITION_BY \"range\", \
             WRITE_EMPTY_FILE false)",
            "Not implemented Error: Can't combine PER_THREAD_OUTPUT and PARTITIONED BY for COPY",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (PER_THREAD_OUTPUT, WRITE_EMPTY_FILE false)",
            "Not implemented Error: Can't combine WRITE_EMPTY_FILE false with PER_THREAD_OUTPUT",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (FILE_SIZE_BYTES)",
            "Binder Error: FILE_SIZE_BYTES cannot be empty",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (FILE_SIZE_BYTES 'abc')",
            "Parser Error: Memory must have a number (e.g. 1GB)",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (FILE_SIZE_BYTES -1)",
            "Binder Error: Unable to parse bytes from \"-1\" for copy option \"FILE_SIZE_BYTES\" ",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (ROW_GROUPS_PER_FILE)",
            "Invalid Input Error: Copy option \"row_groups_per_file\" requires an argument of type \
             UBIGINT",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (ROW_GROUPS_PER_FILE 'x')",
            "Invalid Input Error: Copy option \"row_groups_per_file\" expected an argument of type \
             UBIGINT - the argument \"x\" of type VARCHAR could not be cast as this type",
        ),
        (
            "COPY (FROM range(5)) TO 'e.csv' (ROW_GROUPS_PER_FILE -1)",
            "Invalid Input Error: Copy option \"row_groups_per_file\" expected an argument of type \
             UBIGINT - the argument \"-1\" of type INTEGER could not be cast as this type",
        ),
        (
            "COPY (FROM range(5)) TO 'e.json' (ROW_GROUPS_PER_FILE 1)",
            "Binder Error: Unknown option for COPY ... TO ... (FORMAT JSON): row_groups_per_file.",
        ),
    ] {
        let error = db.execute(sql).expect_err(sql);
        assert_eq!(error.to_string(), want, "{sql}");
    }
}

#[test]
fn return_files_answers_the_count_and_the_files_written_as_the_pin_does() {
    use rudb_common::Value;
    let root = std::env::temp_dir().join(format!("rudb-copy-to-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("makes the directory");
    let dir = root.display().to_string();
    let db = Database::new();
    db.execute("CREATE TABLE t AS SELECT range i, range % 2 k FROM range(5)").expect("creates");
    let files = |sql: &str| {
        let sql = sql.replace("DIR", &dir);
        let result = db.execute(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        assert_eq!(result.names(), ["Count", "Files"], "{sql}");
        assert_eq!(result.changes(), None, "{sql}");
        let Value::List { values, .. } = result.value_at(0, 1) else { panic!("{sql}: a list") };
        let listed = values.iter().map(|value| value.to_string().replace(&dir, "DIR"));
        (result.value_at(0, 0), listed.collect::<Vec<_>>())
    };
    let count = |rows| Value::BigInt(rows);

    assert_eq!(files("COPY t TO 'DIR/a.csv' (RETURN_FILES)"), (count(5), vec!["DIR/a.csv".into()]));
    assert_eq!(
        files("COPY t TO 'DIR/p' (FORMAT csv, PARTITION_BY k, RETURN_FILES TRUE)"),
        (count(5), vec!["DIR/p/k=0/data_0.csv".into(), "DIR/p/k=1/data_0.csv".into()])
    );
    // No rows still write the one file, unless WRITE_EMPTY_FILE says not to.
    assert_eq!(
        files("COPY (FROM t LIMIT 0) TO 'DIR/e.parquet' (RETURN_FILES 1)"),
        (count(0), vec!["DIR/e.parquet".into()])
    );
    assert_eq!(
        files("COPY (FROM t LIMIT 0) TO 'DIR/f.parquet' (RETURN_FILES, WRITE_EMPTY_FILE false)"),
        (count(0), vec![])
    );
    assert_eq!(
        files("COPY (FROM range(5000)) TO 'DIR/r' (FORMAT csv, FILE_SIZE_BYTES 1, RETURN_FILES)"),
        (count(5000), ["0", "1", "2"].map(|i| format!("DIR/r/data_{i}.csv")).to_vec())
    );
    let off = db.execute(&format!("COPY t TO '{dir}/g.csv' (RETURN_FILES false)")).expect("copies");
    assert_eq!(off.changes(), Some(5));

    for (sql, message) in [
        (
            "COPY t TO 'DIR/x.csv' (RETURN_FILES, RETURN_STATS)",
            "Binder Error: Can only set one of RETURN_FILES or RETURN_STATS for COPY",
        ),
        (
            "COPY t TO 'DIR/x.csv' (RETURN_FILES 'x')",
            "Invalid Input Error: Copy option \"return_files\" expected an argument of type \
             BOOLEAN - the argument \"x\" of type VARCHAR could not be cast as this type",
        ),
        (
            "COPY t TO 'DIR/x.csv' (RETURN_STATS)",
            "Not implemented Error: RETURN_STATS is not supported for the \"csv\" copy format",
        ),
        (
            "COPY t TO 'DIR/x.json' (RETURN_STATS)",
            "Not implemented Error: RETURN_STATS is not supported for the \"csv\" copy format",
        ),
    ] {
        let sql = sql.replace("DIR", &dir);
        let error = db.execute(&sql).expect_err(&sql).to_string();
        assert_eq!(error, message, "{sql}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn return_stats_answers_what_each_parquet_file_holds_as_the_pin_does() {
    use rudb_common::Value;
    let root = std::env::temp_dir().join(format!("rudb-copy-to-stats-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("makes the directory");
    let dir = root.display().to_string();
    let db = Database::new();
    db.execute(
        "CREATE TABLE s AS SELECT range i, range % 2 = 0 b, NULLIF(range, 3)::VARCHAR \"a\"\"b\", \
         CASE WHEN range = 1 THEN 'nan'::DOUBLE ELSE range / 2 END f FROM range(5)",
    )
    .expect("creates");
    // The sizes are the writer's own and differ from the pin's, so they are masked.
    let mask = |text: String| {
        let mut out = String::new();
        let mut rest = text.as_str();
        while let Some(at) = rest.find("column_size_bytes=") {
            let start = at + "column_size_bytes=".len();
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            out.push_str(if &rest[..digits] == "0" { "0" } else { "N" });
            rest = &rest[digits..];
        }
        out.push_str(rest);
        out
    };
    let stats = |sql: &str| {
        let sql = sql.replace("DIR", &dir);
        let result = db.execute(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        let names = [
            "filename",
            "count",
            "file_size_bytes",
            "footer_size_bytes",
            "column_statistics",
            "partition_keys",
            "extra_info",
        ];
        assert_eq!(result.names(), names, "{sql}");
        let mut rows = (0..result.len())
            .map(|row| {
                let (Value::UBigInt(size), Value::UBigInt(footer)) =
                    (result.value_at(row, 2), result.value_at(row, 3))
                else {
                    panic!("{sql}: sizes")
                };
                assert!(size > footer && footer > 0, "{sql}: {size} {footer}");
                [0, 1, 4, 5, 6].map(|column| {
                    mask(result.value_at(row, column).to_string().replace(&dir, "DIR"))
                })
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    };

    assert_eq!(
        stats("COPY s TO 'DIR/s.parquet' (RETURN_STATS)"),
        [[
            "DIR/s.parquet".into(),
            "5".into(),
            "{'\"a\"\"b\"'={column_size_bytes=N, max=4, max_is_exact=true, min=0, \
             min_is_exact=true, null_count=1, num_values=5}, '\"b\"'={column_size_bytes=N, max=1, \
             max_is_exact=true, min=0, min_is_exact=true, null_count=0, num_values=5}, \
             '\"f\"'={column_size_bytes=N, has_nan=true, max=2.0, max_is_exact=true, min=0.0, \
             min_is_exact=true, nan_count=1, null_count=0, num_values=5}, \
             '\"i\"'={column_size_bytes=N, max=4, max_is_exact=true, min=0, min_is_exact=true, \
             null_count=0, num_values=5}}"
                .to_string(),
            "NULL".into(),
            "{row_group_count=1}".into(),
        ]]
    );
    // A partitioned copy names the partition of each file and leaves its column out.
    let parts = stats(
        "COPY (SELECT i, b FROM s) TO 'DIR/p' (FORMAT parquet, PARTITION_BY b, RETURN_STATS)",
    );
    let keys = parts.iter().map(|row| row[3].clone()).collect::<Vec<_>>();
    assert_eq!(keys, ["{b=false}", "{b=true}"]);
    assert!(parts[0][2].starts_with("{'\"i\"'=") && !parts[0][2].contains("\"b\""), "{parts:?}");
    // A column with no values has no bounds and no NaN count.
    assert_eq!(
        stats("COPY (SELECT f FROM s LIMIT 0) TO 'DIR/e.parquet' (RETURN_STATS)"),
        [[
            "DIR/e.parquet".to_string(),
            "0".to_string(),
            "{'\"f\"'={column_size_bytes=0, null_count=0, num_values=0}}".to_string(),
            "NULL".to_string(),
            "{row_group_count=0}".to_string(),
        ]]
    );
    let _ = std::fs::remove_dir_all(&root);
}
