//! What the binder produces, checked against the plan's textual form.
//!
//! The assertions are on the printed plan rather than on the arenas, because the printed plan is
//! what a person reads when a query does the wrong thing, and a test that reads what a person
//! reads fails in a way that says what went wrong.

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Field, LogicalType};

use crate::bind_sql;

/// A catalog with two tables in it, which is enough for a join and for every name rule.
fn catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog
        .create_table(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::new("UserID", LogicalType::BigInt),
                Field::new("url", LogicalType::Varchar),
                Field::new("counter", LogicalType::Integer),
            ],
        )
        .expect("a table nothing else has created");
    catalog
        .create_table(
            QualifiedName::new("memory", "main", "visits"),
            vec![
                Field::new("UserID", LogicalType::BigInt),
                Field::new("duration", LogicalType::Integer),
            ],
        )
        .expect("a table nothing else has created");
    catalog
}

/// The printed plan for a query that is expected to bind.
fn plan(query: &str) -> String {
    bind_sql(query, &catalog())
        .unwrap_or_else(|error| panic!("{query} should bind: {error}"))
        .to_string()
}

/// The message for a query that is expected not to bind.
fn failure(query: &str) -> String {
    match bind_sql(query, &catalog()) {
        Ok(plan) => panic!("{query} should not bind, it produced\n{plan}"),
        Err(error) => error.message().to_string(),
    }
}

#[test]
fn a_scan_projects_what_the_query_asked_for() {
    assert_eq!(
        plan("SELECT url FROM hits"),
        "Project #1 [#0.1::VARCHAR AS url]\n  \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn a_star_expands_in_the_order_the_table_has() {
    assert_eq!(
        plan("SELECT * FROM hits"),
        "Project #1 [#0.0::BIGINT AS UserID, #0.1::VARCHAR AS url, #0.2::INTEGER AS counter]\n  \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn a_query_with_no_from_clause_sits_on_one_row() {
    assert_eq!(plan("SELECT 1"), "Project #0 [1::INTEGER AS \"1\"]\n  Dummy\n");
}

#[test]
fn a_where_clause_becomes_a_filter_under_the_projection() {
    assert_eq!(
        plan("SELECT url FROM hits WHERE counter > 5"),
        "Project #1 [#0.1::VARCHAR AS url]\n  \
         Filter (#0.2::INTEGER > 5::INTEGER)::BOOLEAN\n    \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn a_comparison_brings_both_sides_to_the_type_they_meet_at() {
    // The literal is an INTEGER and the column is a BIGINT, so the literal is the one that moves.
    let text = plan("SELECT url FROM hits WHERE UserID = 7");
    assert!(text.contains("(#0.0::BIGINT = CAST(7::INTEGER)::BIGINT)::BOOLEAN"), "{text}");
}

#[test]
fn an_alias_names_the_output_column_and_the_expression_names_it_otherwise() {
    let text = plan("SELECT counter + 1 AS bumped, counter * 2 FROM hits");
    assert!(text.contains("AS bumped"), "{text}");
    assert!(text.contains("AS \"(counter * 2)\""), "{text}");
}

#[test]
fn an_and_of_three_things_is_one_flat_conjunction() {
    let text = plan("SELECT url FROM hits WHERE counter > 1 AND counter < 9 AND url = 'a'");
    assert_eq!(
        text.matches(" AND ").count(),
        2,
        "one conjunction of three, not two of two: {text}"
    );
}

#[test]
fn between_becomes_the_pair_of_comparisons_it_means() {
    let text = plan("SELECT url FROM hits WHERE counter BETWEEN 1 AND 9");
    assert!(text.contains("(#0.2::INTEGER >= 1::INTEGER)"), "{text}");
    assert!(text.contains("(#0.2::INTEGER <= 9::INTEGER)"), "{text}");
    let negated = plan("SELECT url FROM hits WHERE counter NOT BETWEEN 1 AND 9");
    assert!(negated.contains(" OR "), "{negated}");
}

#[test]
fn in_becomes_a_disjunction_of_equalities() {
    let text = plan("SELECT url FROM hits WHERE counter IN (1, 2, 3)");
    assert_eq!(text.matches(" OR ").count(), 2, "{text}");
    let negated = plan("SELECT url FROM hits WHERE counter NOT IN (1, 2)");
    assert!(negated.contains(" AND "), "{negated}");
    assert!(negated.contains("<>"), "{negated}");
}

#[test]
fn is_null_is_the_null_safe_comparison_against_a_null() {
    let text = plan("SELECT url FROM hits WHERE url IS NULL");
    assert!(text.contains("IS NOT DISTINCT FROM"), "{text}");
    let negated = plan("SELECT url FROM hits WHERE url IS NOT NULL");
    assert!(negated.contains("IS DISTINCT FROM"), "{negated}");
}

#[test]
fn a_simple_case_is_bound_as_the_searched_one_it_means() {
    let text = plan("SELECT CASE counter WHEN 1 THEN 'one' ELSE 'many' END AS which FROM hits");
    assert!(text.contains("CASE WHEN"), "{text}");
    assert!(text.contains("(#0.2::INTEGER = 1::INTEGER)"), "{text}");
}

#[test]
fn a_group_by_puts_the_groups_first_and_the_aggregates_after() {
    assert_eq!(
        plan("SELECT url, count(*) FROM hits GROUP BY url"),
        "Project #2 [#1.0::VARCHAR AS url, #1.1::BIGINT AS \"count_star()\"]\n  \
         Aggregate #1 groups=[#0.1::VARCHAR] aggregates=[count_star()::BIGINT]\n    \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn an_aggregate_with_no_group_by_still_aggregates() {
    let text = plan("SELECT sum(counter) FROM hits");
    assert!(
        text.contains("Aggregate #1 groups=[] aggregates=[sum(#0.2::INTEGER)::HUGEINT]"),
        "{text}"
    );
}

#[test]
fn the_same_aggregate_written_twice_is_computed_once() {
    let text = plan("SELECT sum(counter), sum(counter) + 1 FROM hits");
    assert_eq!(text.matches("sum(#").count(), 1, "{text}");
}

#[test]
fn a_column_that_is_neither_grouped_nor_aggregated_is_refused() {
    let message = failure("SELECT url, count(*) FROM hits GROUP BY counter");
    assert!(message.contains("must appear in the GROUP BY clause"), "{message}");
    assert!(message.contains("url"), "the message should name the column: {message}");
}

#[test]
fn a_grouped_expression_is_recognised_wherever_it_is_written_again() {
    let text = plan("SELECT counter + 1, count(*) FROM hits GROUP BY counter + 1");
    assert!(text.contains("groups=[\"+\"(#0.2::INTEGER, 1::INTEGER)::INTEGER]"), "{text}");
    assert!(text.contains("[#1.0::INTEGER AS \"(counter + 1)\""), "{text}");
}

/// `typeof` is answered here, so what reaches the plan is the name of the type. Per #229.
#[test]
fn typeof_is_the_name_of_a_type_rather_than_a_call() {
    let text = plan("SELECT typeof(counter) FROM hits");
    assert!(text.contains("'INTEGER'::VARCHAR AS \"typeof(counter)\""), "{text}");
    // The only `typeof` left in the plan is the column's name, which is the call as it was written.
    assert!(!text.contains("typeof(#"), "nothing is left for the executor to do: {text}");
    // The argument is still an expression where it was written, so it follows the rules about what
    // can be selected next to an aggregate. Upstream refuses this one for the same reason.
    let message = failure("SELECT typeof(url), count(*) FROM hits");
    assert!(message.contains("must appear in the GROUP BY clause"), "{message}");
    let text = plan("SELECT typeof(url) FROM hits GROUP BY url");
    assert!(text.contains("'VARCHAR'::VARCHAR"), "{text}");
}

#[test]
fn group_by_can_name_a_target_by_position_or_by_alias() {
    let by_position = plan("SELECT url, count(*) FROM hits GROUP BY 1");
    let by_alias = plan("SELECT url AS u, count(*) FROM hits GROUP BY u");
    assert!(by_position.contains("groups=[#0.1::VARCHAR]"), "{by_position}");
    assert!(by_alias.contains("groups=[#0.1::VARCHAR]"), "{by_alias}");
}

#[test]
fn group_by_all_groups_everything_that_is_not_an_aggregate() {
    let text = plan("SELECT url, counter, count(*) FROM hits GROUP BY ALL");
    assert!(text.contains("groups=[#0.1::VARCHAR, #0.2::INTEGER]"), "{text}");
}

#[test]
fn having_filters_above_the_aggregate_and_where_filters_below_it() {
    let text = plan("SELECT url FROM hits WHERE counter > 1 GROUP BY url HAVING count(*) > 2");
    let filter_above = text.find("Filter (#1.1").expect("the HAVING filter");
    let aggregate = text.find("Aggregate").expect("the aggregate");
    let filter_below = text.find("Filter (#0.2").expect("the WHERE filter");
    assert!(filter_above < aggregate && aggregate < filter_below, "{text}");
}

#[test]
fn an_aggregate_in_a_where_clause_says_where_it_cannot_go() {
    let message = failure("SELECT url FROM hits WHERE count(*) > 1");
    assert!(message.contains("WHERE clause"), "{message}");
}

#[test]
fn an_order_by_sorts_the_projection_and_takes_the_defaults_sql_gives_it() {
    let text = plan("SELECT url FROM hits ORDER BY url");
    assert!(text.contains("Sort [#1.0::VARCHAR ASC NULLS LAST]"), "{text}");
    let descending = plan("SELECT url FROM hits ORDER BY url DESC");
    assert!(descending.contains("DESC NULLS FIRST"), "{descending}");
}

#[test]
fn an_order_by_on_something_not_selected_projects_it_and_then_drops_it() {
    let text = plan("SELECT url FROM hits ORDER BY counter");
    assert!(text.contains("#1 [#0.1::VARCHAR AS url, #0.2::INTEGER AS counter]"), "{text}");
    assert!(text.starts_with("Project #2 [#1.0::VARCHAR AS url]\n"), "{text}");
}

#[test]
fn an_order_by_position_names_the_output_column() {
    let text = plan("SELECT url, counter FROM hits ORDER BY 2 DESC");
    assert!(text.contains("Sort [#1.1::INTEGER DESC"), "{text}");
    let out_of_range = failure("SELECT url FROM hits ORDER BY 4");
    assert!(out_of_range.contains("out of range"), "{out_of_range}");
}

#[test]
fn a_limit_and_an_offset_are_constants_by_the_time_they_are_here() {
    let text = plan("SELECT url FROM hits LIMIT 10 OFFSET 5");
    assert!(text.contains("Limit 10 offset 5"), "{text}");
    let offset_only = plan("SELECT url FROM hits OFFSET 5");
    assert!(offset_only.contains("Limit ALL offset 5"), "{offset_only}");
}

#[test]
fn distinct_sits_above_the_projection() {
    let text = plan("SELECT DISTINCT url FROM hits");
    assert!(text.starts_with("Distinct on=[]\n  Project"), "{text}");
}

#[test]
fn distinct_cannot_order_by_something_it_does_not_select() {
    let message = failure("SELECT DISTINCT url FROM hits ORDER BY counter");
    assert!(message.contains("must appear in the select list"), "{message}");
}

#[test]
fn two_tables_in_a_from_clause_are_a_cross_product() {
    let text = plan("SELECT hits.url, visits.duration FROM hits, visits");
    assert!(text.contains("CrossProduct"), "{text}");
}

#[test]
fn a_join_condition_binds_against_both_sides() {
    let text = plan("SELECT url FROM hits JOIN visits ON hits.UserID = visits.UserID");
    assert!(text.contains("Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]"), "{text}");
}

#[test]
fn using_makes_the_equality_and_leaves_one_copy_of_the_column() {
    let text = plan("SELECT UserID, url, duration FROM hits JOIN visits USING (UserID)");
    assert!(text.contains("on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]"), "{text}");
    assert!(text.contains("[#0.0::BIGINT AS UserID"), "{text}");
}

#[test]
fn natural_joins_on_whatever_both_sides_call_the_same_thing() {
    let text = plan("SELECT url FROM hits NATURAL JOIN visits");
    assert!(text.contains("Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]"), "{text}");
}

#[test]
fn an_alias_replaces_the_table_name_rather_than_adding_to_it() {
    let text = plan("SELECT h.url FROM hits AS h");
    assert!(text.contains("AS h #0"), "{text}");
    let message = failure("SELECT hits.url FROM hits AS h");
    assert!(message.contains("Referenced table \"hits\" not found"), "{message}");
}

#[test]
fn a_subquery_in_the_from_clause_is_bound_and_then_named() {
    let text = plan("SELECT sub.url FROM (SELECT url FROM hits) AS sub");
    assert!(text.contains("Project #2 [#1.0::VARCHAR AS url]"), "{text}");
}

#[test]
fn a_union_lines_the_two_sides_up_and_sorts_above_both() {
    let text = plan("SELECT counter FROM hits UNION SELECT duration FROM visits ORDER BY 1");
    assert!(text.contains("SetOp UNION DISTINCT"), "{text}");
    assert!(text.starts_with("Sort [#4.0::INTEGER ASC"), "{text}");
    let all = plan("SELECT counter FROM hits UNION ALL SELECT duration FROM visits");
    assert!(all.contains("SetOp UNION ALL"), "{all}");
}

#[test]
fn a_union_of_different_widths_says_so() {
    let message = failure("SELECT url FROM hits UNION SELECT UserID, duration FROM visits");
    assert!(message.contains("same number of result columns"), "{message}");
}

#[test]
fn a_union_of_two_types_casts_the_narrower_side() {
    let text = plan("SELECT UserID FROM hits UNION ALL SELECT duration FROM visits");
    assert!(text.contains("CAST(#3.0::INTEGER)::BIGINT"), "{text}");
}

#[test]
fn a_name_that_is_not_a_table_is_reported_the_way_duckdb_reports_it() {
    let message = failure("SELECT * FROM nope");
    assert!(message.contains("Table with name nope does not exist!"), "{message}");
}

#[test]
fn a_name_that_is_not_a_function_is_reported_the_way_duckdb_reports_it() {
    let message = failure("SELECT nope(url) FROM hits");
    assert!(message.contains("Scalar Function with name nope does not exist!"), "{message}");
}

#[test]
fn arithmetic_on_a_string_is_refused_before_anything_runs() {
    let message = failure("SELECT url + 1 FROM hits");
    assert!(message.contains("No function matches the given name"), "{message}");
}

#[test]
fn a_qualified_table_name_resolves_and_prints_all_three_parts() {
    let text = plan("SELECT url FROM memory.main.hits");
    assert!(text.contains("Get memory.main.hits AS hits"), "{text}");
}

#[test]
fn identifiers_match_without_regard_to_case_and_keep_the_case_they_were_created_with() {
    let text = plan("SELECT USERID FROM HITS");
    assert!(text.contains("AS UserID"), "{text}");
    assert!(text.contains("Get memory.main.hits AS hits"), "{text}");
}

#[test]
fn what_is_not_bound_yet_says_what_was_written_rather_than_producing_a_wrong_plan() {
    for query in [
        "SELECT url FROM hits WHERE counter = (SELECT max(counter) FROM hits)",
        "SELECT counter ** 2 FROM hits",
        "SELECT url FROM hits UNION BY NAME SELECT url FROM hits",
        "SELECT url FROM hits LIMIT 10 PERCENT",
    ] {
        let message = failure(query);
        assert!(!message.is_empty(), "{query} should say what it cannot do");
    }
}

/// `DESCRIBE` is answered while it is bound, so what comes out is a `VALUES` and nothing else.
///
/// The query it describes is bound, because that is the only way to learn the names and the types,
/// and then it is dropped. Nothing reads a row of it. A plan that still had a `Get` under here
/// would be a plan that opens the table to answer a question about the table's shape, which on a
/// hundred million row file is the difference between instant and a minute.
#[test]
fn describe_is_answered_at_bind_time_and_comes_out_as_rows() {
    let text = plan("DESCRIBE SELECT url, counter FROM hits");
    assert!(text.starts_with("Values"), "{text}");
    assert!(!text.contains("Get"), "the described query is bound and then thrown away: {text}");
    assert!(text.contains("'url'") && text.contains("'VARCHAR'"), "{text}");
    assert!(text.contains("'counter'") && text.contains("'INTEGER'"), "{text}");
    // The six columns are an interface: `rudb-compat` reads the types of every result it compares
    // out of `SELECT column_name, column_type FROM (DESCRIBE ...)`, so the names and the order of
    // them are what another program depends on rather than what a person happens to see.
    assert_eq!(
        plan("SELECT * FROM (DESCRIBE SELECT 1 AS a)").lines().next().map(|line| {
            line.split_once('[')
                .map_or(String::new(), |(_, rest)| rest.trim_end_matches(']').into())
        }),
        Some(
            "#1.0::VARCHAR AS column_name, #1.1::VARCHAR AS column_type, #1.2::VARCHAR AS null, \
             #1.3::VARCHAR AS key, #1.4::VARCHAR AS default, #1.5::VARCHAR AS extra"
                .to_string()
        )
    );
}

/// `NO` survives a column being passed through and does not survive anything being done to it.
///
/// That is the reference binary's rule and not an approximation of it. A projection that hands a
/// column straight on cannot introduce a null, and one that computes anything at all can, so the
/// scope carries the flag and the projection copies it only for a bare column reference.
#[test]
fn describe_says_no_for_a_column_that_refuses_nulls_until_something_is_done_to_it() {
    let mut catalog = catalog();
    catalog
        .create_table(
            QualifiedName::new("memory", "main", "strict"),
            vec![Field::required("a", LogicalType::Integer), Field::new("b", LogicalType::Varchar)],
        )
        .expect("a table nothing else has created");
    let says = |query: &str| {
        bind_sql(query, &catalog).unwrap_or_else(|error| panic!("{query}: {error}")).to_string()
    };
    assert!(says("DESCRIBE strict").contains("'NO'"));
    assert!(says("DESCRIBE SELECT * FROM strict").contains("'NO'"));
    assert!(says("DESCRIBE SELECT a FROM strict").contains("'NO'"));
    assert!(!says("DESCRIBE SELECT a + 1 AS c FROM strict").contains("'NO'"));
    // A set operation takes nulls if either side does, whichever side the `NOT NULL` was on.
    assert!(
        !says("DESCRIBE SELECT a FROM strict UNION ALL SELECT counter FROM hits").contains("'NO'")
    );
}

// The statements that are not queries. What is worth asserting here is the resolution: which table
// the name landed on, which column each value goes into, and which type each one arrives as. What
// happens to the catalog afterwards is `rudb`'s test to write, because the binder never touches it.

use crate::{Bound, bind_statement_sql};

/// One statement, bound against the two table catalog.
fn bound(sql: &str) -> Bound {
    bind_statement_sql(sql, &catalog()).unwrap_or_else(|error| panic!("{sql} should bind: {error}"))
}

/// The message for a statement that is expected not to bind.
fn statement_failure(sql: &str) -> String {
    match bind_statement_sql(sql, &catalog()) {
        Ok(_) => panic!("{sql} should not bind"),
        Err(error) => error.message().to_string(),
    }
}

#[test]
fn a_set_arrives_with_its_value_already_a_value() {
    let Bound::Setting(setting) = bound("SET memory_limit = '1GB'") else { panic!("a setting") };
    assert_eq!(setting.name, "memory_limit");
    assert_eq!(setting.scope, rudb_parse::ast::Scope::Unwritten);
    assert_eq!(setting.value, Some(rudb_common::Value::Varchar("1GB".to_string())));
    let Bound::Setting(setting) = bound("SET LOCAL threads = 4") else { panic!("a setting") };
    assert_eq!(setting.scope, rudb_parse::ast::Scope::Local);
    assert_eq!(setting.value, Some(rudb_common::Value::Integer(4)));
    // A reset is the same statement with nothing on the right of it.
    let Bound::Setting(setting) = bound("RESET memory_limit") else { panic!("a setting") };
    assert_eq!(setting.name, "memory_limit");
    assert_eq!(setting.value, None);
}

#[test]
fn a_setting_name_is_not_looked_up_and_a_bare_word_on_the_right_is() {
    // The binder has no idea what settings exist, so a name that is not one gets through here and
    // is refused by the engine. A bare word as the value is a column reference and there is nothing
    // in scope for it to be, which is better than guessing that an unquoted word meant itself.
    let Bound::Setting(setting) = bound("SET nothing_of_the_sort = 1") else { panic!("a setting") };
    assert_eq!(setting.name, "nothing_of_the_sort");
    assert!(
        statement_failure("SET disabled_optimizers = expression_rewriter")
            .contains("expression_rewriter")
    );
}

#[test]
fn a_create_table_resolves_its_name_and_its_types_before_anything_is_created() {
    let Bound::CreateTable(create) = bound("CREATE TABLE s (a DECIMAL(18, 3), b VARCHAR)") else {
        panic!("a create table");
    };
    assert_eq!(create.name, QualifiedName::new("memory", "main", "s"));
    assert_eq!(
        create.columns,
        vec![
            Field::new("a", LogicalType::decimal(18, 3).unwrap()),
            Field::new("b", LogicalType::Varchar),
        ]
    );
    assert!(create.source.is_none());
}

#[test]
fn a_create_table_as_takes_the_query_s_types_and_the_statement_s_names() {
    let Bound::CreateTable(create) = bound("CREATE TABLE s (id) AS SELECT UserID FROM hits") else {
        panic!("a create table");
    };
    assert_eq!(create.columns, vec![Field::new("id", LogicalType::BigInt)]);
    let source = create.source.expect("a create table as has a query");
    assert!(source.to_string().contains("Get"), "{source}");
}

#[test]
fn a_create_table_that_cannot_work_says_so_before_it_is_run() {
    assert_eq!(
        statement_failure("CREATE TABLE s (a INTEGER, A VARCHAR)"),
        "Column with name A already exists!"
    );
    assert_eq!(
        statement_failure("CREATE TABLE s (a, b) AS SELECT UserID FROM hits"),
        "Target table has more colum names than query result."
    );
    assert!(statement_failure("CREATE TABLE s (a NOSUCHTYPE)").contains("NOSUCHTYPE"));
}

#[test]
fn an_insert_projects_the_source_into_the_target_s_shape() {
    let Bound::Insert(insert) = bound("INSERT INTO visits (duration) VALUES (1)") else {
        panic!("an insert");
    };
    assert_eq!(insert.name, QualifiedName::new("memory", "main", "visits"));
    // Two columns out, in the table's order, with the unnamed one a null of the table's own type
    // rather than an untyped null, so the append never has to ask what type the hole is.
    let printed = insert.source.to_string();
    assert!(printed.contains("NULL::BIGINT AS UserID"), "{printed}");
    assert!(printed.contains("AS duration"), "{printed}");

    // And the cast when the source type is not the column's. The literal is an `INTEGER` and
    // `UserID` is a `BIGINT`, so the widening is in the plan and not in the append.
    let Bound::Insert(insert) = bound("INSERT INTO visits (UserID) VALUES (1)") else {
        panic!("an insert");
    };
    let printed = insert.source.to_string();
    assert!(printed.contains("::BIGINT AS UserID"), "{printed}");
}

#[test]
fn an_insert_checks_the_width_and_the_column_names_against_the_table() {
    assert!(statement_failure("INSERT INTO visits VALUES (1)").contains("2 columns"));
    assert!(statement_failure("INSERT INTO visits (nope) VALUES (1)").contains("nope"));
    assert!(
        statement_failure("INSERT INTO visits (duration, duration) VALUES (1, 2)")
            .contains("twice")
    );
    assert!(statement_failure("INSERT INTO nope VALUES (1)").contains("nope"));
}

#[test]
fn a_drop_of_a_name_that_is_not_there_depends_on_if_exists() {
    let Bound::DropTable(drop) = bound("DROP TABLE hits, visits") else { panic!("a drop") };
    assert_eq!(drop.names.len(), 2);
    // With `IF EXISTS` the name that does not resolve is left out rather than kept and forgiven
    // later, so what comes back is a list of drops that all succeed.
    let Bound::DropTable(drop) = bound("DROP TABLE IF EXISTS hits, nope") else { panic!("a drop") };
    assert_eq!(drop.names, vec![QualifiedName::new("memory", "main", "hits")]);
    assert!(statement_failure("DROP TABLE nope").contains("nope"));
}

#[test]
fn values_binds_to_a_values_node_with_the_types_the_rows_agree_on() {
    let Bound::Query(plan) = bound("VALUES (1, 'a'), (2, 'b')") else { panic!("a query") };
    let printed = plan.to_string();
    assert!(printed.starts_with("Values"), "{printed}");
    assert!(printed.contains("col0"), "{printed}");
    assert!(statement_failure("VALUES (1), (2, 3)").contains("same length"));
    assert!(statement_failure("VALUES (1), ('a')").contains("Cannot combine"));
}

#[test]
fn a_table_function_binds_to_its_own_node_and_not_to_a_scan() {
    let printed = plan("SELECT * FROM range(3)");
    assert!(printed.contains("TableFunction range"), "{printed}");
    // The argument is in the plan as an expression and the rows are not, which is the whole reason
    // this is not a Values. A three million row range should be three numbers in a plan dump.
    assert!(printed.contains("args="), "{printed}");
    assert!(!printed.contains("Get"), "{printed}");
}

#[test]
fn an_argument_is_cast_to_the_type_the_function_takes() {
    // `range` takes BIGINT and the literal is an INTEGER, so the cast is written into the plan
    // here rather than decided by the operator at run time.
    let printed = plan("SELECT * FROM range(3)");
    assert!(printed.contains("CAST"), "{printed}");
}

#[test]
fn a_table_function_can_be_aliased_the_same_ways_a_table_can() {
    assert!(plan("SELECT i FROM range(3) t(i)").contains("AS i"));
    assert!(plan("SELECT t.range FROM range(3) t").contains("AS range"));
    // More aliases than columns is the one rule here that does not match DuckDB. DuckDB takes
    // `range(3) t(i, j)` and ignores the second name, and this rejects it, because the check is
    // the same one a table alias goes through and loosening it for one source would be loosening
    // it for all of them. It is written down here rather than left to be found later.
    assert!(failure("SELECT i FROM range(3) t(i, j)").contains("2 columns specified"));
}

#[test]
fn a_name_that_is_not_a_table_function_does_not_fall_through_to_the_table_lookup() {
    // `hits` is a real table in this catalog, so `hits(1)` finding it would be the worst version
    // of this bug rather than the most obvious one.
    assert!(failure("SELECT * FROM hits(1)").contains("hits"));
    assert!(failure("SELECT * FROM nowhere.range(3)").contains("nowhere"));
    assert!(failure("SELECT * FROM range()").contains("range"));
}
