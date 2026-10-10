//! `read_text` and `read_blob`, one row per file with the whole of it in a column.
//!
//! Every expected answer here was taken from the pinned duckdb binary reading the same bytes, with
//! the files under a relative directory there, so a file name is checked here with the directory
//! this test wrote them to in its place.

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
            "rudb-read-text-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("sub")).expect("the scratch directory is made");
        let files: &[(&str, &[u8])] = &[
            ("a.txt", b"hello\nworld\n"),
            ("e.txt", b""),
            ("b.bin", b"\xff\x00x"),
            ("sub/c.txt", b"deep"),
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
        let rows = result
            .rows()
            .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        rows.replace(&format!("{}/", self.dir.display()), "D/")
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
fn each_file_is_a_row_with_its_content_size_and_time() {
    let files = Files::new();
    files.check(&[
        (
            "SELECT filename, content, size, typeof(last_modified) FROM read_text('D/*.txt') \
             ORDER BY 1",
            "D/a.txt|hello\nworld\n|12|TIMESTAMP WITH TIME ZONE\nD/e.txt||0|TIMESTAMP WITH TIME ZONE",
        ),
        ("SELECT filename, content, size FROM read_blob('D/b.bin')", "D/b.bin|\\xFF\\x00x|3"),
        (
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_blob('D/a.txt'))",
            "filename|VARCHAR\ncontent|BLOB\nsize|BIGINT\nlast_modified|TIMESTAMP WITH TIME ZONE",
        ),
        (
            "SELECT last_modified > TIMESTAMPTZ '2020-01-01' FROM read_text('D/a.txt')",
            "true",
        ),
        ("SELECT size FROM read_text('D/b.bin')", "3"),
        ("SELECT filename, content FROM read_text(['D/a.txt'][1])", "D/a.txt|hello\nworld\n"),
    ]);
}

#[test]
fn the_patterns_are_read_in_the_order_given_and_nothing_found_is_no_rows() {
    let files = Files::new();
    files.check(&[
        (
            "SELECT filename FROM read_text(['D/e.txt', 'D/a.txt', 'D/a.txt'])",
            "D/e.txt\nD/a.txt\nD/a.txt",
        ),
        ("SELECT filename FROM read_text(['D/*.txt', 'D/b.bin'])", "D/a.txt\nD/e.txt\nD/b.bin"),
        ("SELECT filename FROM read_text(['D/missing', 'D/a.txt'])", "D/a.txt"),
        ("SELECT filename FROM read_text('D/[ab].*') ORDER BY 1", "D/a.txt\nD/b.bin"),
        ("SELECT count(*) FROM read_blob('D/**')", "4"),
        ("SELECT count(*) FROM read_text('D/missing.txt')", "0"),
        ("SELECT count(*) FROM read_text('D/nothing*.txt', allow_empty=false)", "0"),
        ("SELECT count(*) FROM read_text('D/sub')", "0"),
        ("SELECT count(*) FROM read_text([])", "0"),
    ]);
}

#[test]
fn what_cannot_be_read_is_refused_in_the_pins_words() {
    let files = Files::new();
    for (sql, expected) in [
        (
            "SELECT * FROM read_text('D/b.bin')",
            "Invalid Input Error: read_text: could not read content of file 'D/b.bin' as valid \
             UTF-8 encoded text. You may want to use read_blob instead.",
        ),
        (
            "SELECT * FROM read_text(NULL)",
            "Parser Error: \"read_text\" cannot take NULL list as parameter",
        ),
        (
            "SELECT * FROM read_text(['D/e.txt', NULL])",
            "Parser Error: \"read_text\" reader cannot take NULL input as parameter",
        ),
        (
            "SELECT * FROM read_text([1, 2])",
            "Parser Error: \"read_text\" reader can only take a list of strings, structs or \
             variants as a parameter",
        ),
        (
            "SELECT * FROM read_text('D/a.txt', allow_empty=NULL)",
            "Invalid Input Error: Cannot use NULL as argument for \"allow_empty\"",
        ),
        (
            "SELECT * FROM read_text(42)",
            "Binder Error: No function matches the given name and argument types \
             'read_text(INTEGER)'. You might need to add explicit type casts.\n\tCandidate \
             functions:\n\t\"read_text\"(VARCHAR, allow_empty : BOOLEAN)\n\t\"read_text\"(ANY[], \
             allow_empty : BOOLEAN)\n\t\"read_text\"(VARIANT, allow_empty : BOOLEAN)\n",
        ),
    ] {
        assert_eq!(files.refused(sql), expected, "{sql}");
    }
    assert!(files.refused("SELECT * FROM read_text('D/a.txt', bogus=1)").starts_with(
        "Binder Error: Invalid named parameter \"bogus\" for function read_text\nCandidates:\n    \
             allow_empty BOOLEAN"
    ));
}

#[test]
fn each_reader_lists_three_overloads_in_the_catalog() {
    let files = Files::new();
    assert_eq!(
        files.answered(
            "SELECT function_name, parameter_types FROM duckdb_functions() WHERE function_name IN \
             ('read_text', 'read_blob') ORDER BY function_name"
        ),
        "read_blob|[VARCHAR, BOOLEAN]\nread_blob|['ANY[]', BOOLEAN]\nread_blob|[VARIANT, BOOLEAN]\n\
         read_text|[VARCHAR, BOOLEAN]\nread_text|['ANY[]', BOOLEAN]\nread_text|[VARIANT, BOOLEAN]"
    );
}

#[test]
fn glob_lists_the_files_a_pattern_names_and_reads_none_of_them() {
    let files = Files::new();
    files.check(&[
        ("SELECT file FROM glob('D/*')", "D/a.txt\nD/b.bin\nD/e.txt"),
        ("SELECT file FROM glob('D/**')", "D/a.txt\nD/b.bin\nD/e.txt\nD/sub/c.txt"),
        ("SELECT file FROM glob('D/[ab].*')", "D/a.txt\nD/b.bin"),
        ("SELECT file FROM glob(['D/e.txt', 'D/*.txt', 'D/missing'])", "D/e.txt\nD/a.txt\nD/e.txt"),
        ("SELECT f FROM glob('D/a.txt') t(f)", "D/a.txt"),
        ("SELECT count(*) FROM glob('D/missing/*')", "0"),
        ("SELECT count(*) FROM glob('D/sub')", "0"),
        ("SELECT count(*) FROM glob('')", "0"),
        ("SELECT count(*) FROM glob([])", "0"),
        (
            "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM glob('D/*'))",
            "file|VARCHAR",
        ),
        (
            "SELECT parameters, parameter_types FROM duckdb_functions() WHERE function_name = \
             'glob' ORDER BY 2",
            "[col0]|['ANY[]']\n[col0]|[VARCHAR]\n[col0]|[VARIANT]",
        ),
    ]);
    for (sql, expected) in [
        ("SELECT * FROM glob(NULL)", "Parser Error: \"glob\" cannot take NULL list as parameter"),
        (
            "SELECT * FROM glob(['D/a.txt', NULL])",
            "Parser Error: \"glob\" reader cannot take NULL input as parameter",
        ),
        (
            "SELECT * FROM glob([1])",
            "Parser Error: \"glob\" reader can only take a list of strings, structs or variants as \
             a parameter",
        ),
        (
            "SELECT * FROM glob(42)",
            "Binder Error: No function matches the given name and argument types 'glob(INTEGER)'. \
             You might need to add explicit type casts.\n\tCandidate functions:\n\t\"glob\"\
             (VARCHAR)\n\t\"glob\"(ANY[])\n\t\"glob\"(VARIANT)\n",
        ),
        (
            "SELECT * FROM glob('D/*', x=1)",
            "Binder Error: Invalid named parameter \"x\" for function glob\nFunction does not \
             accept any named parameters.",
        ),
    ] {
        assert_eq!(files.refused(sql), expected, "{sql}");
    }
}
