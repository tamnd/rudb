//! A sort that only fits because it gives the input back as it builds the output.
//!
//! A sort is a pipeline breaker and it holds the rows that arrived until the last one has arrived.
//! What it does not have to do is hold them while it hands the sorted rows out, and until #1298 it
//! did: the assembly that puts the columns in order ran to the end with the input still resident and
//! still charged, so the peak was two copies of the table and a big sort died at the allocator.
//!
//! The assembly lays one column at a time, so the input's copy of a column is finished with the
//! moment that column has been laid. Now it is dropped exactly then and the charge comes down with
//! it, which takes the peak from two copies to one and a column.
//!
//! The test is a memory limit that is enough for one copy and not for two. Two million rows of eight
//! `BIGINT` columns is 128 megabytes of payload, and the numbers below were measured on this
//! machine with both binaries built from the same tree: the sort needed between 340 and 360
//! megabytes before and between 220 and 240 after. The limit here is 300, which is a quarter above
//! what it now needs and an eighth below what it used to.

use rudb::Database;

/// Two million rows of eight columns, sorted on a scrambled key so that no input chunk is already
/// in order and every row moves.
const SORT: &str = "CREATE TABLE s AS SELECT r AS a, (r * 7919) % 1000003 AS b, r + 1 AS c, \
                    r + 2 AS d, r + 3 AS e, r + 4 AS f, r + 5 AS g, r + 6 AS h \
                    FROM range(2000000) AS t(r) ORDER BY (r * 7919) % 1000003, r";

#[test]
fn a_sort_of_two_million_rows_fits_in_a_limit_that_holds_one_copy_of_them() {
    let database = Database::new();
    // One thread, because the limit is a budget for the whole query and how many instances gathered
    // rows should not decide whether it fits.
    database.execute("SET threads=1").expect("sets the thread count");
    database.execute("SET memory_limit='300MB'").expect("sets the limit");
    database.execute(SORT).expect("two million rows sort inside the limit");

    assert_eq!(
        database.value("SELECT count(*) FROM s").expect("counts").to_string(),
        "2000000",
        "every row came through"
    );
    // The first row of the sorted table, which is the row whose scrambled key is smallest. Reading
    // it says the order is the one that was asked for rather than merely that nothing was lost.
    assert_eq!(
        database.value("SELECT min(b) FROM s").expect("reads").to_string(),
        database.value("SELECT b FROM s LIMIT 1").expect("reads").to_string(),
        "the smallest key is first"
    );
}

/// The same two million rows under a limit that one copy of them does not fit in either.
///
/// One copy is 128 megabytes of payload and the sort holds a key and an arrival beside it, so at
/// 100 megabytes it has to spill: it writes what it is holding out as a sorted run, starts again
/// empty, and merges the runs as the rows are read back. Before #1330 this was an out of memory
/// error rather than an answer.
///
/// What is asserted is the order and not just the count, because the interesting way for a merge to
/// be wrong is to give back every row in the wrong sequence. The rows are checked against the run
/// at a limit that does not spill, which is the property that matters: where the spills happened to
/// fall does not change the answer.
#[test]
fn a_sort_that_does_not_fit_spills_and_answers_the_same_thing() {
    let scrambled = "SELECT r AS a, (r * 7919) % 1000003 AS b FROM range(2000000) AS t(r)";
    let ordered = format!("SELECT a, b FROM ({scrambled}) ORDER BY b, a");

    let spilling = Database::new();
    spilling.execute("SET threads=1").expect("sets the thread count");
    spilling.execute("SET memory_limit='100MB'").expect("sets the limit");
    spilling
        .execute(&format!("CREATE TABLE s AS {ordered}"))
        .expect("two million rows sort by spilling");

    let roomy = Database::new();
    roomy.execute("SET threads=1").expect("sets the thread count");
    roomy.execute("SET memory_limit='2GB'").expect("sets the limit");
    roomy.execute(&format!("CREATE TABLE s AS {ordered}")).expect("and again without spilling");

    assert_eq!(
        spilling.value("SELECT count(*) FROM s").expect("counts").to_string(),
        "2000000",
        "every row came through"
    );
    // The limit was for the sort, which has happened. Reading the table back numbers every row,
    // which the window operator does by holding the table, and that is a different operator's
    // bounds and not what this test is about.
    spilling.execute("SET memory_limit='2GB'").expect("raises the limit for the read");
    // A checksum over the whole table in the order it is stored in. Two orders that differ anywhere
    // differ here, and one that is merely a permutation of the right rows does not pass.
    let digest = "SELECT sum(pos * (a * 1000003 + b)) \
                  FROM (SELECT a, b, row_number() OVER () AS pos FROM s)";
    assert_eq!(
        spilling.value(digest).expect("reads").to_string(),
        roomy.value(digest).expect("reads").to_string(),
        "the spilled sort is in the same order as the one that fitted"
    );
}
