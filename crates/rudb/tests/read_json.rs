//! `read_json` and the readers beside it: `read_ndjson`, `read_json_objects`, `read_ndjson_objects`
//! and the `_auto` spellings, and a `.json` file named in a `FROM` clause.
//!
//! Every expected answer here was taken from the pinned duckdb binary reading the same bytes, with
//! the files under a relative directory there, so a message that names a file is checked here with
//! the directory this test wrote them to in its place.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudb::Database;

/// A directory of the files the pin was asked about, written fresh for each test.
struct Files {
    dir: PathBuf,
}

impl Files {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rudb-read-json-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the scratch directory is made");
        let files: &[(&str, &[u8])] = &[
            ("nd.json", b"{\"a\":1,\"b\":\"x\"}\n{\"a\":2,\"c\":[1,2]}\n"),
            (
                "ty.json",
                b"{\"d\":\"2024-01-02\",\"t\":\"2024-01-02 03:04:05\",\"u\":\"8d3c0b2e-1a2b-4c3d-8e9f-0a1b2c3d4e5f\",\"n\":null,\"o\":{\"x\":1}}\n\
                  {\"d\":\"2024-02-03\",\"t\":\"2024-01-02T03:04:05\",\"u\":\"8d3c0b2e-1a2b-4c3d-8e9f-0a1b2c3d4e5f\",\"n\":null,\"o\":{\"x\":2,\"y\":\"z\"}}\n",
            ),
            ("tz.json", b"{\"t\":\"2024-01-02T03:04:05+01:00\"}\n{\"t\":\"2024-01-02T03:04:05+02:00\"}\n"),
            ("dfmt.json", b"{\"d\":\"01-02-2024\"}\n{\"d\":\"13-02-2024\"}\n"),
            ("arr.json", b"[{\"a\":1,\"b\":true},{\"a\":2.5}]"),
            ("bad.json", b"{\"a\":1}\nnot json\n{\"a\":2}\n"),
            ("m1.json", b"{\"a\":1}\n{\"a\":2}\n"),
            ("m2.json", b"{\"a\":\"x\",\"b\":1}\n"),
            ("mix.json", b"{\"a\":1}\n{\"a\":\"x\"}\n"),
            ("obj.json", b"{\"x\":{\"k1\":1,\"k2\":2}}\n{\"x\":{\"k3\":3}}\n"),
            ("sc.json", b"1\n\"s\"\n"),
            ("un.json", b"{\"a\":1}{\"a\":2}\n  {\"a\":\n3}\n"),
            ("va.json", b"[1,2,3]"),
            ("u1.json", b"{\"a\":1,\"b\":true}\n{\"a\":2,\"b\":false}\n"),
            ("u2.json", b"{\"c\":\"x\",\"a\":1.5,\"b\":7}\n"),
            ("u3.json", b"{\"a\":\"2020-01-01\",\"s\":{\"x\":1}}\n"),
            ("u4.json", b"{\"s\":{\"y\":\"q\"},\"A\":3}\n"),
            ("rows.json.gz", include_bytes!("../../rudb-compress/tests/data/rows.gz")),
        ];
        for (name, bytes) in files {
            std::fs::write(dir.join(name), bytes).expect("a fixture is written");
        }
        Self { dir }
    }

    /// `sql` with `D/` standing for the directory.
    fn sql(&self, sql: &str) -> String {
        sql.replace("D/", &format!("{}/", self.dir.display()))
    }

    fn answered(&self, sql: &str) -> String {
        let sql = self.sql(sql);
        let result =
            Database::new().query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
        result
            .rows()
            .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn refused(&self, sql: &str) -> String {
        let sql = self.sql(sql);
        let error = Database::new().query(&sql).expect_err(&sql).to_string();
        error.replace(&format!("{}/", self.dir.display()), "D/")
    }

    fn check(&self, cases: &[(&str, &str)]) {
        for (sql, expected) in cases {
            assert_eq!(self.answered(sql), *expected, "{sql}");
        }
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_columns_and_their_types_are_the_ones_the_pin_detects() {
    let files = Files::new();
    files.check(&[
        ("SELECT a, b, c FROM read_json('D/nd.json') ORDER BY a", "1|x|NULL\n2|NULL|[1, 2]"),
        (
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_json('D/nd.json'))",
            "a|BIGINT\nb|VARCHAR\nc|BIGINT[]",
        ),
        (
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_json('D/ty.json'))",
            "d|DATE\nt|VARCHAR\nu|UUID\nn|\"NULL\"\no|STRUCT(x BIGINT, y VARCHAR)",
        ),
        (
            "SELECT d, t, o.x FROM read_json('D/ty.json') ORDER BY d",
            "2024-01-02|2024-01-02 03:04:05|1\n2024-02-03|2024-01-02T03:04:05|2",
        ),
        ("SELECT typeof(t) FROM read_json('D/tz.json') LIMIT 1", "TIMESTAMP WITH TIME ZONE"),
        ("SELECT * FROM read_json('D/arr.json')", "1.0|true\n2.5|NULL"),
        ("SELECT * FROM read_json('D/sc.json')", "1\n\"s\""),
        ("SELECT * FROM read_json('D/va.json')", "1\n2\n3"),
        ("SELECT * FROM 'D/m1.json'", "1\n2"),
    ]);
}

#[test]
fn a_date_reads_by_the_format_given_or_the_one_detected() {
    let files = Files::new();
    for sql in [
        "SELECT typeof(d), d FROM read_json('D/dfmt.json', dateformat='%d-%m-%Y') ORDER BY d",
        "SELECT typeof(d), d FROM read_json('D/dfmt.json') ORDER BY d",
    ] {
        assert_eq!(files.answered(sql), "DATE|2024-02-01\nDATE|2024-02-13", "{sql}");
    }
}

#[test]
fn the_layout_of_the_documents_is_detected_or_given() {
    let files = Files::new();
    files.check(&[
        ("SELECT * FROM read_json('D/un.json', format='unstructured')", "1\n2\n3"),
        (
            "SELECT * FROM read_json_objects('D/nd.json')",
            "{\"a\":1,\"b\":\"x\"}\n{\"a\":2,\"c\":[1,2]}",
        ),
        ("SELECT * FROM read_json('D/bad.json', ignore_errors=true)", "1\nNULL\n2"),
        (
            "SELECT * FROM read_json('D/bad.json', format='newline_delimited', ignore_errors=true)",
            "1\nNULL\n2",
        ),
    ]);
    assert_eq!(
        files.refused("SELECT * FROM read_ndjson_objects('D/un.json')"),
        "Invalid Input Error: Malformed JSON in file \"D/un.json\", at byte 8 in line 2: unexpected \
         content after document. Try auto-detecting the JSON format"
    );
}

#[test]
fn several_files_are_read_into_the_columns_of_all_of_them() {
    let files = Files::new();
    files.check(&[
        (
            "SELECT * FROM read_json(['D/m1.json', 'D/m2.json']) ORDER BY ALL",
            "\"x\"|1\n1|NULL\n2|NULL",
        ),
        ("SELECT * FROM read_json('D/m1.json', columns={a:'VARCHAR', z:'INT'})", "1|NULL\n2|NULL"),
    ]);
    assert_eq!(
        files.refused("SELECT * FROM read_json(['D/m1.json', 'D/mix.json'], columns={a:'INT'})"),
        "Invalid Input Error: JSON transform error in file \"D/mix.json\", in line 2: Failed to cast \
         value to numerical: \"x\"\nTry setting 'auto_detect' to true, specifying 'format' or \
         'records' manually, or setting 'ignore_errors' to true."
    );
}

#[test]
fn the_filename_column_is_the_path_as_written() {
    let files = Files::new();
    let path = files.sql("D/m1.json");
    assert_eq!(
        files.answered("SELECT a, filename FROM read_json('D/m1.json', filename=true)"),
        format!("1|{path}\n2|{path}")
    );
    assert_eq!(
        files.refused("SELECT * FROM read_json('D/m1.json', filename='a')"),
        "Binder Error: Option filename adds column \"a\", but a column with this name is also in the \
         file. Try setting a different name: filename='<filename column name>'"
    );
}

#[test]
fn a_gzip_file_is_read_by_its_extension_and_a_wrong_compression_says_so() {
    let files = Files::new();
    assert_eq!(
        files.answered("SELECT count(*), sum(id) FROM read_json('D/rows.json.gz')"),
        "40|780"
    );
    assert_eq!(
        files.refused("SELECT count(*) FROM read_json('D/m1.json', compression='gzip')"),
        "IO Error: Input is not a GZIP stream: D/m1.json"
    );
    assert_eq!(
        files.refused("SELECT count(*) FROM read_json('D/m1.json', compression='zstd')"),
        "IO Error: Unknown frame descriptor"
    );
    assert_eq!(
        files.refused("SELECT count(*) FROM read_json('D/m1.json', compression='lz4')"),
        "Not implemented Error: Attempting to open a compressed file, but the compression type is \
         not supported (compression type \"lz4\")"
    );
}

#[test]
fn the_named_parameters_are_refused_in_the_pins_words() {
    let files = Files::new();
    assert_eq!(
        files.refused("SELECT * FROM read_json('D/bad.json')"),
        "Invalid Input Error: Malformed JSON in file \"D/bad.json\", at byte 1 in line 3: invalid \
         literal. "
    );
    assert!(
        files
            .refused("SELECT * FROM read_json('D/nd.json', bogus=1)")
            .starts_with("Binder Error: Invalid named parameter \"bogus\" for function read_json\nCandidates:\n    allow_empty BOOLEAN\n    array BOOLEAN\n")
    );
    for (sql, expected) in [
        (
            "SELECT * FROM read_json_auto('D/nd.json', sample_size='x')",
            "Invalid Input Error: Failed to cast value: Could not convert string 'x' to INT64",
        ),
        (
            "SELECT * FROM read_json('D/nd.json', columns=3)",
            "Binder Error: read_json \"columns\" parameter requires a struct as input.",
        ),
        (
            "SELECT * FROM read_json('D/nd.json', format='nope')",
            "Binder Error: format must be one of ['nd', 'array', 'newline_delimited', \
             'unstructured', 'auto'], not 'nope'",
        ),
        (
            "SELECT * FROM read_json('D/nd.json', records='nope')",
            "Binder Error: read_json requires \"records\" to be one of ['auto', 'true', 'false'].",
        ),
    ] {
        assert_eq!(files.refused(sql), expected, "{sql}");
    }
}

#[test]
fn each_reader_lists_three_overloads_in_the_catalog() {
    let files = Files::new();
    assert_eq!(
        files.answered("SELECT count(*) FROM duckdb_functions() WHERE function_name = 'read_json'"),
        "3"
    );
}

#[test]
fn read_single_json_file_reads_one_file_named_as_it_is() {
    let files = Files::new();
    files.check(&[
        ("SELECT * FROM read_single_json_file('D/m1.json')", "1\n2"),
        ("SELECT a FROM read_single_json_file('D/nd.json') WHERE a > 1", "2"),
        ("SELECT * FROM read_single_json_file('D/m1.json', columns={a: 'VARCHAR'})", "1\n2"),
        (
            "SELECT count(*) FROM duckdb_functions() WHERE function_name = 'read_single_json_file'",
            "1",
        ),
    ]);
    assert_eq!(
        files.refused("SELECT * FROM read_single_json_file('D/m*.json')"),
        "IO Error: Cannot open file \"D/m*.json\": No such file or directory"
    );
    assert!(files.refused("SELECT * FROM read_single_json_file(['D/m1.json'])").starts_with(
        "Binder Error: No function matches the given name and argument types \
                 'read_single_json_file(VARCHAR[])'. You might need to add explicit type casts.\n\
                 \tCandidate functions:\n\t\"read_single_json_file\"(VARCHAR, \
                 convert_strings_to_integers : BOOLEAN, maximum_sample_files : BIGINT, "
    ));
    assert!(
        files
            .refused("SELECT * FROM read_single_json_file('D/m1.json', filename=true)")
            .starts_with(
                "Binder Error: Invalid named parameter \"filename\" for function \
                 read_single_json_file\nCandidates:\n    array BOOLEAN\n    auto_detect BOOLEAN\n"
            )
    );
    assert_eq!(
        files.refused("SELECT * FROM read_single_json_file('D/m1.json', auto_detect=false)"),
        "Binder Error: When auto_detect=false, read_json requires columns to be specified through \
         the \"columns\" parameter."
    );
}

#[test]
fn union_by_name_samples_every_file_and_merges_the_columns_by_name() {
    let files = Files::new();
    files.check(&[
        (
            "SELECT * FROM read_json(['D/u1.json', 'D/u2.json'], union_by_name=true) ORDER BY ALL",
            "1.0|true|NULL\n1.5|7|x\n2.0|false|NULL",
        ),
        (
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_json(['D/u1.json', \
             'D/u2.json'], union_by_name=true, maximum_sample_files=1))",
            "a|DOUBLE\nb|JSON\nc|VARCHAR",
        ),
        (
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_json(['D/u3.json', \
             'D/u4.json'], union_by_name=true))",
            "a|DATE\ns|STRUCT(x BIGINT, y VARCHAR)\nA_1|BIGINT",
        ),
        (
            "SELECT * FROM read_json(['D/u3.json', 'D/u4.json'], union_by_name=true)",
            "2020-01-01|{'x': 1, 'y': NULL}|NULL\nNULL|{'x': NULL, 'y': q}|3",
        ),
        (
            "SELECT * FROM read_json(['D/u1.json', 'D/va.json'], union_by_name=true) LIMIT 1",
            "{\"a\":1,\"b\":true}",
        ),
        (
            "SELECT * FROM read_json(['D/u2.json', 'D/u1.json'], union_by_name=1)",
            "x|1.5|7\nNULL|1.0|true\nNULL|2.0|false",
        ),
        (
            "SELECT * FROM read_json_objects(['D/u2.json', 'D/u1.json'], union_by_name=true)",
            "{\"c\":\"x\",\"a\":1.5,\"b\":7}\n{\"a\":1,\"b\":true}\n{\"a\":2,\"b\":false}",
        ),
    ]);
    assert_eq!(
        files.refused("SELECT * FROM read_json(['D/u2.json', 'D/u1.json'], union_by_name='x')"),
        "Invalid Input Error: Failed to cast value: Could not convert string 'x' to BOOL"
    );
}
