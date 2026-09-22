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
