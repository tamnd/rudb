//! The planner's row count for an equality, taken from the distinct counts in a Parquet footer.
//!
//! A Parquet writer that built a dictionary for a column of a row group can say how many entries it
//! put in it, and DuckDB says so. That number is exact for the row group and the estimator wants it
//! for the column, which is a different question: the column holds at least as many distinct values
//! as its largest row group does and no more than all of them added up. So a file of one row group
//! answers the question exactly and a file of several brackets it, and every test here is about
//! which of those two happened and what the plan said about it.
//!
//! `zoned.rs` is the same shape of test for the minimum and the maximum, which is the other half of
//! what a footer holds. The two meet at `EXPLAIN`, where the provenance on a line says which of them
//! the number came from, and a filter whose fraction came from both says propagation instead.
//!
//! Every test has two halves, an estimate and the truth, for the reason `zoned.rs` gives: an
//! estimate below the truth is the failure that does not look like anything until a plan is built on
//! it. `counted.parquet` is written in one row group on purpose and its README entry says so.

use rudb::Database;
use rudb_common::{LogicalType, Value};

/// The one row group fixture, where a stated count is a count of the column.
fn one_group() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/counted.parquet");
    format!("'{path}'")
}

/// The two row group fixture, where a stated count brackets the column instead.
fn two_groups() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/mixed.parquet");
    format!("'{path}'")
}

/// The three row group fixture, one of whose columns is stated by only two of the three.
fn three_groups() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/zstd.parquet");
    format!("'{path}'")
}

/// The plan text for a query.
fn explained(database: &Database, sql: &str) -> String {
    let result = database.query(sql).expect("the explain ran");
    assert_eq!(result.types(), [LogicalType::Varchar, LogicalType::Varchar]);
    match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    }
}

/// The line of the plan the filter is on, which is where the estimate for it is printed.
fn filter_line(text: &str) -> String {
    text.lines()
        .find(|line| line.trim_start().starts_with("Filter "))
        .unwrap_or_else(|| panic!("no filter in {text}"))
        .to_string()
}

/// The estimate the planner prints for that filter over `file`.
fn planned(database: &Database, file: &str, predicate: &str) -> String {
    let sql = format!("EXPLAIN SELECT 1 FROM read_parquet({file}) WHERE {predicate}");
    filter_line(&explained(database, &sql))
}

/// How many rows the filter really keeps in `file`.
fn counted(database: &Database, file: &str, predicate: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM read_parquet({file}) WHERE {predicate}");
    let result = database.query(&sql).expect("the count ran");
    match result.value_at(0, 0) {
        Value::BigInt(count) => count,
        other => panic!("the count came back as {other:?}"),
    }
}

/// The Statistics section of an `EXPLAIN (STATISTICS)`, which is where reads are counted.
fn statistics(database: &Database, sql: &str) -> String {
    let text = explained(database, &format!("EXPLAIN (STATISTICS) {sql}"));
    let (_, section) = text.split_once("\nStatistics\n").unwrap_or_else(|| panic!("{text}"));
    section.to_string()
}

#[test]
fn a_file_of_one_row_group_counted_the_column_and_an_equality_divides_by_the_count() {
    // The whole file is one row group, so what the writer counted for the group is what the column
    // holds, and the estimate is the textbook one value out of ninety seven. Two thousand rows over
    // ninety seven is twenty against a truth of twenty one, where the constant this replaced said
    // four hundred, nineteen times over.
    let database = Database::new();
    let line = planned(&database, &one_group(), "few = 5");
    assert!(line.contains("[~20 rows estimated from dictionary]"), "{line}");
    assert_eq!(counted(&database, &one_group(), "few = 5"), 21);
}

#[test]
fn the_count_lands_exactly_where_the_values_really_are_spread_evenly() {
    // `label` holds five values and two thousand rows are spread over them four hundred each, which
    // is the uniformity assumption being true rather than assumed. The estimate is four hundred and
    // so is the truth. The constant said four hundred as well and was right here by coincidence,
    // which is the case worth having a test for: it is the fifth landing on a column of five values.
    let database = Database::new();
    let line = planned(&database, &one_group(), "label = 'tag3'");
    assert!(line.contains("[~400 rows estimated from dictionary]"), "{line}");
    assert_eq!(counted(&database, &one_group(), "label = 'tag3'"), 400);
}

#[test]
fn a_column_the_writer_built_no_dictionary_for_is_left_to_the_constant() {
    // `many` is two thousand different values in two thousand rows, so DuckDB gave up on the
    // dictionary and stated no count. Nothing is guessed in its place: the estimate is the fifth it
    // always was, which is four hundred against a truth of one. That is the gap this box does not
    // close and it is here so the gap is written down rather than implied.
    let database = Database::new();
    let line = planned(&database, &one_group(), "many = 5");
    assert!(line.contains("[~400 rows estimated from default]"), "{line}");
    assert_eq!(counted(&database, &one_group(), "many = 5"), 1);
}

#[test]
fn a_file_of_two_row_groups_takes_the_larger_group_as_a_lower_bound_on_the_column() {
    // Both groups state ninety seven, so the column holds at least ninety seven and at most a
    // hundred and ninety four, and the lower end is the one used. Here the lower end is the truth:
    // the column really does hold ninety seven values and both groups hold all of them. The
    // estimator cannot know that from the footer, which is why the number is a certified bound and
    // not a count, and the estimate it gives is forty two against a truth of forty three.
    let database = Database::new();
    let line = planned(&database, &two_groups(), "a = 5");
    assert!(line.contains("[~42 rows estimated from dictionary]"), "{line}");
    assert_eq!(counted(&database, &two_groups(), "a = 5"), 43);
}

#[test]
fn the_lower_end_of_the_bracket_is_the_safe_one_to_divide_by() {
    // The other direction on the same file. Dividing by the lower end keeps more rows than dividing
    // by the upper end would, and too many rows costs a scan while too few costs the wrong build
    // side. `s` holds five values in a file of two groups that each state five, so the upper end is
    // ten and would have said four hundred and nine rows against a truth of seven hundred and two.
    // The lower end says eight hundred and nineteen, which is over the truth rather than under it.
    let database = Database::new();
    let line = planned(&database, &two_groups(), "s = 'tag3'");
    assert!(line.contains("[~819 rows estimated from dictionary]"), "{line}");
    assert_eq!(counted(&database, &two_groups(), "s = 'tag3'"), 702);
}

#[test]
fn one_row_group_that_stated_nothing_gives_up_the_column_however_many_others_stated() {
    // `b` in the zstd fixture is stated by two of its three row groups, because the third is the
    // short tail group and the writer laid it out differently. A group that said nothing could hold
    // anything, so neither end of the bracket holds and the column falls back to the constant. The
    // alternative is to add up the groups that did state one and call the result a bound, which it
    // is not: the unstated group could hold a thousand values nobody else has.
    let database = Database::new();
    let line = planned(&database, &three_groups(), "b = 5");
    assert!(line.contains("[~4000 rows estimated from default]"), "{line}");
    assert_eq!(counted(&database, &three_groups(), "b = 5"), 0);

    // And the column of the same file that every group did state comes through, so the refusal is
    // about the column and not about the file.
    let line = planned(&database, &three_groups(), "a = 5");
    assert!(line.contains("[~206 rows estimated from dictionary]"), "{line}");
    assert_eq!(counted(&database, &three_groups(), "a = 5"), 207);
}

#[test]
fn explain_statistics_counts_the_distinct_counts_and_says_what_class_they_came_back_in() {
    // `spec/stats/05-every-query.md` section 5.1.1 asks every read to declare what it is for, and a
    // distinct count is read to decide: it chooses between plans that produce the same rows and it
    // never changes an answer. So it lands on the same line as the cardinalities, and the classes
    // underneath are what tell the two apart.
    //
    // One row group makes the count exact, so this reads two exact numbers, the file's rows and the
    // column's count, and two estimates, which are the filter and the projection over it.
    let database = Database::new();
    let exact = statistics(
        &database,
        &format!("SELECT 1 FROM read_parquet({}) WHERE few = 5", one_group()),
    );
    assert!(
        exact.contains("4 read to decide: exact 2, certified 0, estimated 2, unknown 0"),
        "{exact}"
    );
    assert!(exact.contains("nothing was read to answer or to enable"), "{exact}");

    // Two row groups make the same count certified rather than exact, which is the whole of the
    // difference between the two files and is visible here and nowhere else in the output.
    let bracketed =
        statistics(&database, &format!("SELECT 1 FROM read_parquet({}) WHERE a = 5", two_groups()));
    assert!(
        bracketed.contains("4 read to decide: exact 1, certified 1, estimated 2, unknown 0"),
        "{bracketed}"
    );

    // A column nobody counted is read and found to be nothing, and the read is counted anyway. A
    // section that left the misses out would say this plan was built on three numbers when it was
    // built on two, and finding the column nobody counted is most of what somebody reads it for.
    let missing = statistics(
        &database,
        &format!("SELECT 1 FROM read_parquet({}) WHERE many = 5", one_group()),
    );
    assert!(
        missing.contains("4 read to decide: exact 1, certified 0, estimated 2, unknown 1"),
        "{missing}"
    );
    assert!(missing.contains("75% of them with a number behind them"), "{missing}");
}

#[test]
fn a_join_reads_a_count_from_each_side_of_every_condition() {
    // The join arithmetic divides the product of the two sides by how many key values they share,
    // and the shared count is the larger of the two columns' counts. Both are read whether or not
    // both answer, so a self join on a counted column is four reads: a cardinality for each scan
    // and a distinct count for each side of the one condition.
    let database = Database::new();
    let file = one_group();
    let sql = format!(
        "SELECT 1 FROM read_parquet({file}) l JOIN read_parquet({file}) r ON l.few = r.few"
    );
    let section = statistics(&database, &sql);
    assert!(section.contains("exact 4"), "{section}");

    // Ninety seven values shared between two sides of two thousand rows is about forty one thousand
    // pairs, where the containment assumption on its own would have said two thousand. The truth is
    // forty one thousand two hundred and sixty, because the rows do not divide evenly over the
    // values and the sixty values that got an extra row contribute an extra pair each side of it.
    let plan = explained(&database, &format!("EXPLAIN {sql}"));
    let join = plan
        .lines()
        .find(|line| line.trim_start().starts_with("Join "))
        .unwrap_or_else(|| panic!("no join in {plan}"));
    assert!(join.contains("[~41237 rows"), "{join}");
    let result = database.query(&format!("SELECT count(*) FROM ({sql}) t")).expect("the count ran");
    assert_eq!(result.value_at(0, 0), Value::BigInt(41260));
}
