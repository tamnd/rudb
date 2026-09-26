//! `COPY ... TO` a CSV file, checked byte for byte against what the pinned DuckDB wrote for the
//! same statements.

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
        ("COPY (SELECT 1 AS x) TO 'o.json'", "FORMAT json is not supported yet"),
        ("COPY (SELECT 1 AS x) TO 'o.csv' (FORMAT parquet)", "FORMAT parquet is not supported yet"),
    ] {
        let error = db.execute(sql).expect_err(sql).to_string();
        assert!(error.starts_with("Not implemented Error"), "{sql} gave {error}");
        assert!(error.contains(message), "{sql} gave {error}");
    }
}
