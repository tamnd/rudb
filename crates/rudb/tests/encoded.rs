//! The encoded forms our own file hands out answer what the same rows answer in memory.
//!
//! A column written to a native file does not come back flat. A run of numbers with a narrow range
//! comes back bit packed, and one with few distinct values comes back as a dictionary whose values
//! are a packed run, and those are the two shapes every numeric column of a stored table arrives
//! in. Every kernel has a path for each of them and each of those paths is a second implementation
//! of the same arithmetic, written against codes instead of values, so each one is a place for the
//! answer to change while nobody is looking at it.
//!
//! The table in memory is the oracle here. It holds the same rows in flat vectors and answers out
//! of the paths that have been there the longest, so asking both sides the same question and
//! comparing is asking whether the encoding bought speed or bought a wrong answer.
//!
//! The wrong answer that started this file is `AVG` over a decimal column of a stored table. A
//! packed run holds the unscaled integer, so 0.99 is held as 99, and the mean was being added up
//! out of those integers without the scale ever being taken off. A column of cents came back a
//! hundred times too large, and it came back that way silently, because the sum next to it was
//! right and so was the count. The assertions below check the mean against a number worked out by
//! hand as well as against memory, since the point of a hand written number is that it does not
//! move when both sides move together.

use rudb::Database;
use rudb_common::Value;

/// How far apart two doubles are allowed to be before the test calls them different answers.
///
/// Not zero, because the two sides add the rows up in a different order: memory folds one chunk at
/// a time and the file folds one stripe at a time, and floating point addition does not care what
/// the rows are so much as what order they arrived in. The bugs this file is for are off by a
/// factor of a power of ten, so anything near the last bit is noise and anything this catches is
/// real.
const CLOSE_ENOUGH: f64 = 1e-9;

/// The rows both sides hold, written once so that neither side can be built from a different table.
///
/// `cents` is the shape that broke: a hundred distinct values between zero and one, at scale two,
/// which is small enough a range to pack and few enough values to hold as a dictionary over the
/// packing. `price` is the same type with nearly ten thousand distinct values in it, which is the
/// shape a real price column has. `low` and `high` are there for a comparison between two encoded
/// columns rather than between one and a constant, which is a different path again.
const ROWS: &str = "INSERT INTO t SELECT (i % 100)::DECIMAL(15,2) / 100, \
     (i % 9701)::DECIMAL(15,2) / 100, (i % 31) + 1000, (i % 17) + 1000 \
     FROM range(20000) tbl(i)";

/// The same rows in memory and in a file, with the file's answer checked against the memory one.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-encoded-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let create = "CREATE TABLE t(cents DECIMAL(15,2), price DECIMAL(15,2), \
             low BIGINT, high BIGINT)";
        let memory = Database::new();
        memory.execute(create).expect("the memory table is created");
        memory.execute(ROWS).expect("the memory table is filled");
        let name = path.to_str().expect("a UTF-8 temporary path");
        // Written by one database and read by another, because a table is only backed by the file
        // once the database that wrote it has gone. The writer still has the rows in memory, so
        // asking it anything would be asking the same side of this test twice.
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing.execute(create).expect("the file table is created");
            writing.execute(ROWS).expect("the file table is filled");
            writing.execute("CHECKPOINT").expect("the file table is committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        Self { memory, file, path }
    }

    /// Asserts the file agrees with memory, and returns what they both said.
    fn agree(&self, query: &str) -> Value {
        let wanted = self.memory.value(query).expect("the memory table answers");
        let got = self.file.value(query).expect("the file answers");
        match (&got, &wanted) {
            (Value::Double(got), Value::Double(wanted)) => {
                assert!(
                    (got - wanted).abs() <= wanted.abs() * CLOSE_ENOUGH,
                    "the file says {got} and memory says {wanted} for {query}"
                );
            }
            _ => assert_eq!(got, wanted, "the file and memory disagree about {query}"),
        }
        got
    }

    /// The same as above for a query that answers with a double, as the number itself.
    fn number(&self, query: &str) -> f64 {
        match self.agree(query) {
            Value::Double(number) => number,
            other => panic!("{query} answered with {other:?} rather than a double"),
        }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Asserts a number is the one worked out by hand rather than one that merely matches the oracle.
#[track_caller]
fn about(got: f64, wanted: f64, what: &str) {
    assert!((got - wanted).abs() <= wanted.abs() * CLOSE_ENOUGH, "{what} came back as {got}");
}

/// Every aggregate over the two encoded shapes, against memory and against arithmetic.
///
/// The hand written numbers are the twenty thousand rows of `i % 100` divided by a hundred: every
/// value from 0.00 to 0.99 appears two hundred times, so the total is 9900.00, the mean is 0.495,
/// the smallest is 0.00 and the largest is 0.99.
#[test]
fn an_aggregate_over_a_stored_column_of_numbers_is_the_answer_the_same_rows_give_in_memory() {
    let pair = Pair::new("aggregate");
    assert_eq!(pair.agree("SELECT count(*) FROM t"), Value::BigInt(20000));
    assert_eq!(pair.agree("SELECT count(cents) FROM t"), Value::BigInt(20000));
    // The width is not checked here. A total of a `DECIMAL(15, 2)` column is declared at the width
    // of the column rather than widened to thirty eight the way DuckDB widens it, which is #1068
    // and is a question about the type rather than about the encoding, and both sides of this test
    // answer it the same way.
    match pair.agree("SELECT sum(cents) FROM t") {
        Value::Decimal { unscaled, scale, .. } => {
            assert_eq!((unscaled, scale), (990_000, 2), "the total of a stored decimal column");
        }
        other => panic!("a total came back as {other:?}"),
    }
    assert_eq!(
        pair.agree("SELECT min(cents) FROM t"),
        Value::Decimal { unscaled: 0, width: 15, scale: 2 }
    );
    assert_eq!(
        pair.agree("SELECT max(cents) FROM t"),
        Value::Decimal { unscaled: 99, width: 15, scale: 2 }
    );
    about(pair.number("SELECT avg(cents) FROM t"), 0.495, "the mean of a stored decimal column");
    // The wide column as well as the narrow one, since how many distinct values a column has is
    // what decides which of the two shapes it is stored in and neither shape is the special case.
    about(pair.number("SELECT avg(price) FROM t"), 47.139_101_5, "the mean of a stored price");
    pair.agree("SELECT sum(price) FROM t");
    pair.agree("SELECT min(price) FROM t");
    pair.agree("SELECT max(price) FROM t");
    pair.agree("SELECT sum(low) FROM t");
    pair.agree("SELECT min(low) FROM t");
    pair.agree("SELECT max(low) FROM t");
}

/// The mean of a stored decimal column against the sum and the count of the same column.
///
/// The failure this is for cannot be seen by looking at one number. A mean a hundred times too
/// large looks like a plausible mean, and it sits next to a sum and a count that are both right, so
/// what says it is wrong is that the three of them stop being consistent with each other.
#[test]
fn the_mean_of_a_stored_decimal_column_is_its_total_divided_by_its_rows() {
    let pair = Pair::new("mean");
    for column in ["cents", "price"] {
        let mean = pair.number(&format!("SELECT avg({column}) FROM t"));
        let total = pair.number(&format!("SELECT sum({column})::DOUBLE FROM t"));
        let rows = match pair.agree(&format!("SELECT count({column}) FROM t")) {
            Value::BigInt(rows) => rows as f64,
            other => panic!("a count came back as {other:?}"),
        };
        about(mean, total / rows, &format!("the mean of {column}"));
    }
}

/// A filter over the encoded forms, both against a constant and between two encoded columns.
#[test]
fn a_filter_over_stored_columns_keeps_the_rows_the_same_filter_keeps_in_memory() {
    let pair = Pair::new("filter");
    pair.agree("SELECT count(*) FROM t WHERE cents < 0.50");
    pair.agree("SELECT count(*) FROM t WHERE cents >= 0.50");
    pair.agree("SELECT count(*) FROM t WHERE low < high");
    pair.agree("SELECT count(*) FROM t WHERE low = high");
    pair.agree("SELECT sum(cents) FROM t WHERE low < high");
    pair.agree("SELECT sum(price) FROM t WHERE cents > 0.90 AND low < high");
}

/// Arithmetic over a stored decimal column, which is a cast before it is anything else.
///
/// A decimal that meets a literal or another decimal is widened first, and the widening reads the
/// stored form, so this is the cast path over a packed run and over a dictionary of one rather than
/// the aggregate path.
#[test]
fn arithmetic_over_a_stored_decimal_column_is_what_the_same_arithmetic_gives_in_memory() {
    let pair = Pair::new("arithmetic");
    pair.agree("SELECT sum(1 - cents) FROM t");
    pair.agree("SELECT sum(cents * 2) FROM t");
    pair.agree("SELECT sum(price * (1 - cents)) FROM t");
    pair.agree("SELECT sum(cents::DOUBLE) FROM t");
    about(
        pair.number("SELECT avg(price * (1 - cents)) FROM t"),
        pair.number("SELECT sum(price * (1 - cents))::DOUBLE / count(*) FROM t"),
        "the mean of a product of two stored decimal columns",
    );
}

/// A grouped aggregate over the encoded forms, where the key is encoded as well as the value.
#[test]
fn a_grouped_aggregate_over_stored_columns_adds_up_to_what_it_adds_up_to_in_memory() {
    let pair = Pair::new("grouped");
    pair.agree("SELECT count(*) FROM (SELECT low FROM t GROUP BY low) g");
    pair.agree("SELECT sum(total) FROM (SELECT low, sum(cents) AS total FROM t GROUP BY low) g");
    pair.agree("SELECT sum(rows) FROM (SELECT low, count(*) AS rows FROM t GROUP BY low) g");
    about(
        pair.number("SELECT sum(mean) FROM (SELECT low, avg(cents) AS mean FROM t GROUP BY low) g"),
        pair.number(
            "SELECT sum(mean) FROM (SELECT low, sum(cents)::DOUBLE / count(*) AS mean \
             FROM t GROUP BY low) g",
        ),
        "the means of the groups of a stored decimal column",
    );
}
