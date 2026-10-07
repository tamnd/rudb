//! The transform of this crate against the DuckDB transform of `rudb-parse`.
//!
//! Each statement here is read the same way by the two dialects, so the two transforms must build
//! the same tree, which `rudb_parse::shape` writes out with every field. A cast names its type with
//! `pg_catalog`, because the PostgreSQL grammar writes the types of SQL that way and the DuckDB
//! grammar keeps a type name as it was written.
//!
//! Set `RUDB_PG_TRANSFORM_CORPUS` to a file of statements, one on each line, to compare the two
//! transforms on all of them. The test then writes each statement whose trees are not the same and
//! counts the ones that the new transform does not build yet.

use rudb_common::session::IdentifierCase;
use rudb_parse::shape;
use rudb_pgparse::transform::{Refused, transform};

const SAME: &[&str] = &[
    "select 1, 'a', 1.5, null, true, false, -1, -1.5, +2, - a, 10000000000",
    "select a from t where a in (1, 2) and b not in (3) or not c",
    "select * from t1 join t2 on t1.a = t2.a left join t3 using (a) cross join t4, t5 natural full join t6",
    "select t.*, x.y.z, a.b.c.d from s.t",
    "select a from (select 1 as a) as q (b)",
    "select generate_series(1, 3), * from generate_series(1, 3) as g (x)",
    "select sum(a) over (partition by b order by c rows between 1 preceding and current row) from t",
    "select sum(a) over w, rank() over (w order by b) from t window w as (partition by c)",
    "select count(distinct a), count(*) filter (where a > 1) from t group by b having count(*) > 1",
    "select a from t order by a desc nulls first, b asc nulls last limit 10 offset 5",
    "select a from t union all select b from u intersect select c from v except select d from w",
    "values (1, 'a'), (2, 'b')",
    "with q as (select 1 as a) select * from q",
    "with q as (select 1 as a) select * from q, q as r",
    "with q (x) as materialized (select 1) select * from q",
    "with q as not materialized (select 1) select * from q, q as r",
    "with recursive r (n) as (select 1 union all select n + 1 from r where n < 3) select * from r",
    "with q as (select 1 as a) select * from (with q as (select 2 as a) select * from q) as s, q",
    "select case when a then 1 when b then 2 else 3 end, case a when 1 then 'x' end from t",
    "select a between 1 and 2, a not between 1 and 2, a is null, a is not null, a is true, a is not unknown from t",
    "select a like 'x', a not ilike 'y', a similar to 'z', a not similar to 'z', a like 'x' escape '!', a not like 'x' escape '!', a ~ 'r', a !~* 's' from t",
    "select exists (select 1), not exists (select 1), (select 1), a in (select 1), a not in (select 1), a = any (select 1), a < all (select 1), array(select 1) from t",
    "select a = any (array[1, 2]), array[1, 2], array[[1], [2]], row(1, 2), (1, 2) from t",
    "select a::pg_catalog.int4, cast(a as pg_catalog.varchar(10)), a::pg_catalog.numeric(10,2)[], a::\"MyType\", a::s.t from t",
    "select $1, $2 from t",
    "select coalesce(a, b), nullif(a, b), greatest(a, b), least(a, b) from t",
    "select a[1], a[1:2], a[:2], (a).b, a collate \"C\" from t",
    "select current_date, current_timestamp, current_user, localtime(3), session_user from t",
    "select extract(year from a), substring(a from 1 for 2), position('a' in b), trim(both 'x' from a), trim(leading from a), overlay(a placing 'b' from 1 for 2), a at time zone 'UTC' from t",
    "select string_agg(a, ',' order by a) from t",
    "select distinct a from t",
    "select distinct on (a) a, b from t",
    "select a from t fetch first 3 rows only",
    "select a from t limit all",
    "select a is distinct from b, a is not distinct from b, a || b, a % b, a & b, a << 1, ~ a from t",
    "select count(*) from t as x",
    "select * from (values (1), (2)) as v (a)",
    "select a from t where a > 1 order by 1",
    "select lower(a) from t group by 1",
    "select 1 where false",
    "select (select 1) as a order by a",
    "select sum(a) over (order by b range between unbounded preceding and unbounded following) from t",
    "select sum(a) over (order by b groups between 1 following and 3 following exclude ties) from t",
    "select sum(a) over (order by b rows 2 preceding exclude current row) from t",
    "select a from t1 left join (t2 join t3 on true) on true",
    "select * from t1 right join t2 on true, lateral (select 1) as l",
    "select 1 as \"Foo\", \"Bar\" from t",
    "select a -> 'b', a ->> 'b', a @> b, a <@ b, a && b, a ^@ 'x' from t",
];

/// The shape of the tree of each transform, or the error of each.
fn shapes(sql: &str) -> (String, Result<String, Refused>) {
    let old = match rudb_parse::parse_ast_postgres(sql, IdentifierCase::Lower) {
        Ok(ast) => shape::script(&ast),
        Err(error) => format!("error: {error}"),
    };
    (old, transform(sql).map(|ast| shape::script(&ast)))
}

#[test]
fn both_transforms_build_the_same_tree() {
    let mut differ = Vec::new();
    for sql in SAME {
        match shapes(sql) {
            (old, Ok(new)) if old == new => {}
            (old, new) => differ.push(format!("{sql}\n  old {old}\n  new {new:?}")),
        }
    }
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

#[test]
fn corpus() {
    let Ok(path) = std::env::var("RUDB_PG_TRANSFORM_CORPUS") else {
        return;
    };
    let text = std::fs::read_to_string(&path).expect("the corpus file reads");
    let (mut same, mut differ, mut not_yet) = (0, 0, std::collections::BTreeMap::new());
    for sql in text.lines().filter(|line| !line.trim().is_empty()) {
        match shapes(sql) {
            (old, Ok(new)) if old == new => same += 1,
            (_, Err(Refused::NotYet(node))) => *not_yet.entry(node).or_insert(0) += 1,
            (old, new) => {
                differ += 1;
                println!("{sql}\n  old {old}\n  new {new:?}");
            }
        }
    }
    println!("same {same}, differ {differ}, not yet {not_yet:?}");
}
