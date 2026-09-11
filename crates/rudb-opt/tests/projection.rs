//! Projection pushdown, written as text in and text out.
//!
//! Every test here reads a plan out of a string, optimizes it, and compares the printed result with
//! another string. That is on purpose. A test that builds a plan with the arena API and then asserts
//! on node references is a test of the arena, and it stops being readable at about four nodes. A
//! test that reads and prints is a test of the rewrite and it stays readable at forty, which is the
//! size a ClickBench plan actually is.
//!
//! The text is also the thing a person looks at when a query is slow, so a test that asserts on it
//! is asserting on the same thing the person will read.

use rudb_opt::optimize;
use rudb_plan::Plan;

/// Read `before`, optimize, and demand the printed result is `after`.
fn rewrites(before: &str, after: &str) {
    let plan = Plan::parse(before).unwrap_or_else(|error| panic!("cannot read:\n{before}{error}"));
    assert_eq!(plan.to_string(), before, "the input is not what the printer would have written");
    let out = optimize(&plan).expect("the plan under test optimizes");
    assert_eq!(out.to_string(), after);
    out.validate().expect("the rewritten plan is valid");
    assert_eq!(plan.to_string(), before, "the pass changed the plan it was handed");
}

/// Read `text`, optimize, and demand nothing moved.
fn leaves_alone(text: &str) {
    rewrites(text, text);
}

#[test]
fn a_scan_reads_the_one_column_the_query_asks_for_and_the_reference_follows_it() {
    rewrites(
        "\
Project #1 [#0.2::BIGINT AS c]
  Get memory.main.hits AS hits #0 [a::VARCHAR, b::VARCHAR, c::BIGINT]
",
        "\
Project #1 [#0.0::BIGINT AS c]
  Get memory.main.hits AS hits #0 [c::BIGINT]
",
    );
}

/// The case that started this. `SELECT count(*)` names no column, so the scan reads none, and the
/// 105 columns of a ClickBench partition stop being read to answer a question about row counts.
#[test]
fn a_scan_nothing_reads_from_comes_out_with_no_columns_at_all() {
    rewrites(
        "\
Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]
  Get memory.main.hits AS hits #0 [a::VARCHAR, b::BIGINT]
",
        "\
Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]
  Get memory.main.hits AS hits #0 []
",
    );
}

/// A plan that reads everything it scans is the case the pass has to be free in, since it is what
/// every `SELECT *` is, and a rewrite that reordered a column list it did not need to touch would
/// be a diff in every plan test in the repository for nothing.
#[test]
fn a_plan_that_uses_every_column_is_left_the_way_it_was() {
    leaves_alone(
        "\
Project #1 [#0.0::VARCHAR AS a, #0.1::BIGINT AS b]
  Get memory.main.hits AS hits #0 [a::VARCHAR, b::BIGINT]
",
    );
}

/// The columns keep the order the scan had, rather than the order the query mentioned them in. A
/// reader that had to seek backwards through a Parquet file because the plan asked for column 40
/// before column 3 would read the same bytes in a worse order.
#[test]
fn the_surviving_columns_stay_in_the_order_the_scan_had_them() {
    rewrites(
        "\
Project #1 [#0.3::BIGINT AS d, #0.1::VARCHAR AS b]
  Get memory.main.hits AS hits #0 [a::VARCHAR, b::VARCHAR, c::BIGINT, d::BIGINT]
",
        "\
Project #1 [#0.1::BIGINT AS d, #0.0::VARCHAR AS b]
  Get memory.main.hits AS hits #0 [b::VARCHAR, d::BIGINT]
",
    );
}

/// A column read only by a filter is a used column, which is the whole reason the pass walks every
/// expression of every node rather than just the projection at the root.
#[test]
fn a_column_that_only_a_filter_reads_is_a_column_the_scan_still_reads() {
    rewrites(
        "\
Project #1 [#0.0::VARCHAR AS a]
  Filter (#0.2::BIGINT > 3::BIGINT)::BOOLEAN
    Get memory.main.hits AS hits #0 [a::VARCHAR, b::VARCHAR, c::BIGINT]
",
        "\
Project #1 [#0.0::VARCHAR AS a]
  Filter (#0.1::BIGINT > 3::BIGINT)::BOOLEAN
    Get memory.main.hits AS hits #0 [a::VARCHAR, c::BIGINT]
",
    );
}

/// A column buried in a `CASE` inside a function call is still read, because the expression walk
/// goes all the way to the leaves.
#[test]
fn a_column_nested_deep_inside_an_expression_is_still_found() {
    rewrites(
        "\
Project #1 [upper(CASE WHEN (#0.2::BIGINT > 3::BIGINT)::BOOLEAN THEN #0.0::VARCHAR ELSE ''::VARCHAR END::VARCHAR)::VARCHAR AS a]
  Get memory.main.hits AS hits #0 [a::VARCHAR, b::VARCHAR, c::BIGINT]
",
        "\
Project #1 [upper(CASE WHEN (#0.1::BIGINT > 3::BIGINT)::BOOLEAN THEN #0.0::VARCHAR ELSE ''::VARCHAR END::VARCHAR)::VARCHAR AS a]
  Get memory.main.hits AS hits #0 [a::VARCHAR, c::BIGINT]
",
    );
}

/// Two scans narrow independently, and the one that loses nothing keeps its list. A pass that
/// renumbered by a running total across the plan rather than per table index would get this wrong
/// in a way that a single scan test cannot see.
#[test]
fn each_side_of_a_join_narrows_on_its_own() {
    rewrites(
        "\
Project #2 [#0.2::BIGINT AS c, #1.0::BIGINT AS k]
  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]
    Get memory.main.left AS left #0 [j::BIGINT, unused::VARCHAR, c::BIGINT]
    Get memory.main.right AS right #1 [k::BIGINT]
",
        "\
Project #2 [#0.1::BIGINT AS c, #1.0::BIGINT AS k]
  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]
    Get memory.main.left AS left #0 [j::BIGINT, c::BIGINT]
    Get memory.main.right AS right #1 [k::BIGINT]
",
    );
}

/// A table function narrows the same way a table does, which is the arm that matters for
/// `read_parquet`, since a file on disk is where the bytes a projection saves actually are.
#[test]
fn a_table_function_narrows_the_way_a_table_does() {
    rewrites(
        "\
Project #1 [#0.1::BIGINT AS b]
  TableFunction read_parquet args=['hits.parquet'::VARCHAR] #0 [a::VARCHAR, b::BIGINT]
",
        "\
Project #1 [#0.0::BIGINT AS b]
  TableFunction read_parquet args=['hits.parquet'::VARCHAR] #0 [b::BIGINT]
",
    );
}

/// A `VALUES` list is already in the plan, so narrowing one saves reading nothing and would cost a
/// rewrite of every row. The pass leaves it alone on purpose rather than by omission.
#[test]
fn a_values_list_keeps_its_columns_even_when_nothing_reads_them() {
    leaves_alone(
        "\
Project #1 [#0.0::BIGINT AS a]
  Values #0 [a::BIGINT, b::BIGINT] rows=[[1::BIGINT, 2::BIGINT], [3::BIGINT, 4::BIGINT]]
",
    );
}

/// A column a sort key reads is used even though it never reaches the output, which is what
/// `ORDER BY` on a column the query does not select means.
#[test]
fn a_column_only_a_sort_key_reads_survives() {
    rewrites(
        "\
Project #1 [#0.0::VARCHAR AS a]
  Sort [#0.2::BIGINT DESC NULLS LAST]
    Get memory.main.hits AS hits #0 [a::VARCHAR, b::VARCHAR, c::BIGINT]
",
        "\
Project #1 [#0.0::VARCHAR AS a]
  Sort [#0.1::BIGINT DESC NULLS LAST]
    Get memory.main.hits AS hits #0 [a::VARCHAR, c::BIGINT]
",
    );
}
