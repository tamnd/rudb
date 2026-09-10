//! The dump and the reader have to be a fixed point, or the textual form is a debugging aid rather
//! than a format.
//!
//! Every test here goes the same way. Build a plan with the public API, print it, read the text
//! back, print that, and demand the two dumps are the same string. A field that the printer forgets
//! or that the reader guesses wrong shows up as a diff between two dumps rather than as a wrong
//! answer three layers later. The unit tests next to each module cover the pieces; these cover the
//! shapes that a real query produces, which is where the pieces have to agree with each other.

use rudb_common::{Field, LogicalType, Value};
use rudb_plan::{
    Arm, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
    SetOpKind, SortKey, StrRef,
};

/// Print, read, print, and insist the two dumps agree. Returns the dump so a test can also assert
/// on the text itself.
fn round_trips(plan: &Plan) -> String {
    plan.validate().expect("the plan under test is valid before it is printed");
    let first = plan.to_string();
    let read = Plan::parse(&first)
        .unwrap_or_else(|error| panic!("cannot read back:\n{first}\nbecause {error}"));
    let second = read.to_string();
    assert_eq!(first, second, "the dump changed on the way through the reader");
    first
}

/// Read, print, read, and insist the two texts agree. The other direction, for dumps that a person
/// wrote rather than the printer.
fn reads_back(text: &str) -> Plan {
    let plan = Plan::parse(text).unwrap_or_else(|error| panic!("cannot read:\n{text}\n{error}"));
    assert_eq!(plan.to_string(), text, "the text is not what the printer would have written");
    plan
}

fn column(plan: &mut Plan, table: u32, index: u32, ty: LogicalType) -> ExprRef {
    plan.add_expr(Expr::Column(ColumnBinding::new(table, index)), ty)
}

fn varchar(plan: &mut Plan, text: &str) -> ExprRef {
    plan.add_constant(Value::Varchar(text.to_string()))
}

fn names(plan: &mut Plan, list: &[&str]) -> rudb_plan::Slice {
    let refs: Vec<StrRef> = list.iter().map(|name| plan.intern(name)).collect();
    plan.add_name_list(&refs)
}

/// A scan of `memory.main.<table>` under its own name, with the given columns.
fn get(plan: &mut Plan, table: &str, index: u32, columns: &[(&str, LogicalType)]) -> NodeRef {
    let catalog = plan.intern("memory");
    let schema = plan.intern("main");
    let name = plan.intern(table);
    let fields: Vec<Field> =
        columns.iter().map(|(field, ty)| Field::new(*field, ty.clone())).collect();
    let columns = plan.add_fields(&fields);
    plan.add_node(Node::Get { catalog, schema, table: name, alias: name, index, columns })
}

/// ClickBench Q13, which is the example in the crate documentation. The dump is asserted character
/// for character, because that example is what a reader of the crate takes the format to be and a
/// silent drift between the two is a documentation bug that no compiler catches.
#[test]
fn the_shape_of_a_grouped_top_n_survives_the_round_trip() {
    let mut plan = Plan::new();
    let scan = get(&mut plan, "hits", 0, &[("SearchPhrase", LogicalType::Varchar)]);

    let phrase = column(&mut plan, 0, 0, LogicalType::Varchar);
    let empty = varchar(&mut plan, "");
    let not_empty = plan.add_expr(
        Expr::Compare { op: CompareOp::NotEqual, left: phrase, right: empty },
        LogicalType::Boolean,
    );
    let filter = plan.add_node(Node::Filter { input: scan, predicate: not_empty });

    let group = column(&mut plan, 0, 0, LogicalType::Varchar);
    let groups = plan.add_expr_list(&[group]);
    let count_star = plan.intern("count_star");
    let no_args = plan.add_expr_list(&[]);
    let count = plan.add_expr(
        Expr::Aggregate { name: count_star, args: no_args, distinct: false, filter: None },
        LogicalType::BigInt,
    );
    let aggregates = plan.add_expr_list(&[count]);
    let aggregate = plan.add_node(Node::Aggregate { input: filter, index: 1, groups, aggregates });

    let by_count = column(&mut plan, 1, 1, LogicalType::BigInt);
    let keys =
        plan.add_sort_keys(&[SortKey { expr: by_count, descending: true, nulls_first: false }]);
    let sort = plan.add_node(Node::Sort { input: aggregate, keys });
    let limit = plan.add_node(Node::Limit { input: sort, count: Some(10), offset: 0 });

    let out_phrase = column(&mut plan, 1, 0, LogicalType::Varchar);
    let out_count = column(&mut plan, 1, 1, LogicalType::BigInt);
    let exprs = plan.add_expr_list(&[out_phrase, out_count]);
    let labels = names(&mut plan, &["SearchPhrase", "c"]);
    let project = plan.add_node(Node::Project { input: limit, index: 2, exprs, names: labels });
    plan.set_root(project);

    let expected = "\
Project #2 [#1.0::VARCHAR AS SearchPhrase, #1.1::BIGINT AS c]
  Limit 10 offset 0
    Sort [#1.1::BIGINT DESC NULLS LAST]
      Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]
        Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN
          Get memory.main.hits AS hits #0 [SearchPhrase::VARCHAR]
";
    assert_eq!(round_trips(&plan), expected);
}

/// Two joins, so the reader has to keep a stack rather than a single pending child, and an `OR`
/// under an `AND` so nesting inside a conjunction is exercised.
#[test]
fn a_three_way_join_survives_the_round_trip() {
    let mut plan = Plan::new();
    let orders = get(
        &mut plan,
        "orders",
        0,
        &[("o_orderkey", LogicalType::BigInt), ("o_custkey", LogicalType::BigInt)],
    );
    let lineitem = get(
        &mut plan,
        "lineitem",
        1,
        &[("l_orderkey", LogicalType::BigInt), ("l_price", LogicalType::Double)],
    );
    let customer = get(
        &mut plan,
        "customer",
        2,
        &[("c_custkey", LogicalType::BigInt), ("c_name", LogicalType::Varchar)],
    );

    let left_key = column(&mut plan, 0, 0, LogicalType::BigInt);
    let right_key = column(&mut plan, 1, 0, LogicalType::BigInt);
    let on_order = plan.add_expr(
        Expr::Compare { op: CompareOp::Equal, left: left_key, right: right_key },
        LogicalType::Boolean,
    );
    let conditions = plan.add_expr_list(&[on_order]);
    let inner = plan.add_node(Node::Join {
        left: orders,
        right: lineitem,
        kind: JoinKind::Inner,
        conditions,
    });

    let cust_key = column(&mut plan, 0, 1, LogicalType::BigInt);
    let other_key = column(&mut plan, 2, 0, LogicalType::BigInt);
    let on_customer = plan.add_expr(
        Expr::Compare { op: CompareOp::Equal, left: cust_key, right: other_key },
        LogicalType::Boolean,
    );
    let conditions = plan.add_expr_list(&[on_customer]);
    let outer = plan.add_node(Node::Join {
        left: inner,
        right: customer,
        kind: JoinKind::Left,
        conditions,
    });

    let price = column(&mut plan, 1, 1, LogicalType::Double);
    let cheap = plan.add_constant(Value::Double(10.0));
    let dear = plan.add_constant(Value::Double(1000.0));
    let low = plan.add_expr(
        Expr::Compare { op: CompareOp::Less, left: price, right: cheap },
        LogicalType::Boolean,
    );
    let high = plan.add_expr(
        Expr::Compare { op: CompareOp::Greater, left: price, right: dear },
        LogicalType::Boolean,
    );
    let either = plan.add_expr_list(&[low, high]);
    let extreme = plan.add_expr(
        Expr::Conjunction { op: ConjunctionOp::Or, children: either },
        LogicalType::Boolean,
    );
    let name = column(&mut plan, 2, 1, LogicalType::Varchar);
    let unnamed = varchar(&mut plan, "");
    let named = plan.add_expr(
        Expr::Compare { op: CompareOp::DistinctFrom, left: name, right: unnamed },
        LogicalType::Boolean,
    );
    let both = plan.add_expr_list(&[extreme, named]);
    let predicate = plan.add_expr(
        Expr::Conjunction { op: ConjunctionOp::And, children: both },
        LogicalType::Boolean,
    );
    let filter = plan.add_node(Node::Filter { input: outer, predicate });
    plan.set_root(filter);

    let dump = round_trips(&plan);
    assert!(dump.contains("Join INNER on="), "the inner join is not in\n{dump}");
    assert!(dump.contains("Join LEFT on="), "the outer join is not in\n{dump}");
    assert!(dump.contains(" OR "), "the disjunction is not in\n{dump}");
}

/// A `VALUES` list, a `CASE` over it, and a cast, all in one plan. `VALUES` is the only operator
/// whose arguments are a list of lists, so it is the one that finds a reader that splits on commas
/// without counting brackets.
#[test]
fn values_and_case_survive_the_round_trip() {
    let mut plan = Plan::new();
    let fields = [Field::new("n", LogicalType::Integer), Field::new("label", LogicalType::Varchar)];
    let columns = plan.add_fields(&fields);

    let mut rows = Vec::new();
    for (number, label) in [(1, "one"), (2, "two, with a comma"), (3, "three")] {
        let n = plan.add_constant(Value::Integer(number));
        let text = varchar(&mut plan, label);
        rows.push(plan.add_expr_list(&[n, text]));
    }
    let rows = plan.add_rows(&rows);
    let values = plan.add_node(Node::Values { index: 0, columns, rows });

    let n = column(&mut plan, 0, 0, LogicalType::Integer);
    let one = plan.add_constant(Value::Integer(1));
    let is_one = plan.add_expr(
        Expr::Compare { op: CompareOp::Equal, left: n, right: one },
        LogicalType::Boolean,
    );
    let first = varchar(&mut plan, "first");
    let two = plan.add_constant(Value::Integer(2));
    let is_two = plan.add_expr(
        Expr::Compare { op: CompareOp::Equal, left: n, right: two },
        LogicalType::Boolean,
    );
    let second = varchar(&mut plan, "second");
    let arms =
        plan.add_arms(&[Arm { when: is_one, then: first }, Arm { when: is_two, then: second }]);
    let rest = varchar(&mut plan, "later");
    let case = plan.add_expr(Expr::Case { arms, otherwise: Some(rest) }, LogicalType::Varchar);

    let widened = plan.add_expr(Expr::Cast { input: n, try_cast: false }, LogicalType::BigInt);
    let label = column(&mut plan, 0, 1, LogicalType::Varchar);
    let doubtful = plan.add_expr(Expr::Cast { input: label, try_cast: true }, LogicalType::Integer);

    let exprs = plan.add_expr_list(&[widened, case, doubtful]);
    let labels = names(&mut plan, &["n", "ordinal", "maybe"]);
    let project = plan.add_node(Node::Project { input: values, index: 1, exprs, names: labels });
    plan.set_root(project);

    let dump = round_trips(&plan);
    assert!(dump.contains("CASE WHEN "), "the case is not in\n{dump}");
    assert!(dump.contains("TRY_CAST("), "the try cast is not in\n{dump}");
    assert!(dump.contains("'two, with a comma'"), "the comma is not in\n{dump}");
}

/// A set operation over two scans, with a `DISTINCT ON` above it and a positional join beside it.
/// The point is the operators whose arguments are keywords rather than expressions.
#[test]
fn the_keyword_operators_survive_the_round_trip() {
    let mut plan = Plan::new();
    let left = get(&mut plan, "a", 0, &[("x", LogicalType::Integer)]);
    let right = get(&mut plan, "b", 1, &[("x", LogicalType::Integer)]);
    let union =
        plan.add_node(Node::SetOp { left, right, kind: SetOpKind::Union, all: true, index: 2 });

    let x = column(&mut plan, 2, 0, LogicalType::Integer);
    let on = plan.add_expr_list(&[x]);
    let distinct = plan.add_node(Node::Distinct { input: union, on });

    let other = get(&mut plan, "c", 3, &[("y", LogicalType::Integer)]);
    let positional = plan.add_node(Node::Join {
        left: distinct,
        right: other,
        kind: JoinKind::Positional,
        conditions: rudb_plan::Slice::EMPTY,
    });
    let cross = get(&mut plan, "d", 4, &[("z", LogicalType::Integer)]);
    let product = plan.add_node(Node::CrossProduct { left: positional, right: cross });
    let all = plan.add_node(Node::Limit { input: product, count: None, offset: 25 });
    plan.set_root(all);

    let dump = round_trips(&plan);
    assert!(dump.contains("SetOp UNION ALL #2"), "the set operation is not in\n{dump}");
    assert!(dump.contains("Distinct on="), "the distinct is not in\n{dump}");
    assert!(dump.contains("Join POSITIONAL on=[]"), "the positional join is not in\n{dump}");
    assert!(dump.contains("Limit ALL offset 25"), "the open limit is not in\n{dump}");
}

/// Every aggregate feature at once, in the one slot an aggregate is allowed to appear in.
#[test]
fn a_distinct_aggregate_with_a_filter_survives_the_round_trip() {
    let mut plan = Plan::new();
    let scan = get(
        &mut plan,
        "hits",
        0,
        &[("UserID", LogicalType::BigInt), ("IsRefresh", LogicalType::Boolean)],
    );

    let user = column(&mut plan, 0, 0, LogicalType::BigInt);
    let refresh = column(&mut plan, 0, 1, LogicalType::Boolean);
    let args = plan.add_expr_list(&[user]);
    let count = plan.intern("count");
    let distinct_users = plan.add_expr(
        Expr::Aggregate { name: count, args, distinct: true, filter: Some(refresh) },
        LogicalType::BigInt,
    );

    let count_star = plan.intern("count_star");
    let no_args = plan.add_expr_list(&[]);
    let filtered_total = plan.add_expr(
        Expr::Aggregate { name: count_star, args: no_args, distinct: false, filter: Some(refresh) },
        LogicalType::BigInt,
    );

    let aggregates = plan.add_expr_list(&[distinct_users, filtered_total]);
    let groups = plan.add_expr_list(&[]);
    let aggregate = plan.add_node(Node::Aggregate { input: scan, index: 1, groups, aggregates });
    plan.set_root(aggregate);

    let dump = round_trips(&plan);
    assert!(
        dump.contains("count(DISTINCT #0.0::BIGINT FILTER #0.1::BOOLEAN)"),
        "the distinct filtered count is not in\n{dump}"
    );
    assert!(
        dump.contains("count_star(FILTER #0.1::BOOLEAN)"),
        "the filtered count star is not in\n{dump}"
    );
}

/// A constant of every value shape the executor can hold, plus a typed null of a type that has no
/// non-null value yet. A new `Value` variant that nobody teaches the printer about fails here,
/// because the printer's fallback for an unknown variant is text the reader cannot read.
#[test]
fn a_constant_of_every_shape_survives_the_round_trip() {
    let mut plan = Plan::new();
    let dummy = plan.add_node(Node::Dummy);

    let constants = [
        Value::Boolean(true),
        Value::Boolean(false),
        Value::TinyInt(i8::MIN),
        Value::SmallInt(i16::MIN),
        Value::Integer(i32::MIN),
        Value::BigInt(i64::MIN),
        Value::HugeInt(i128::MIN),
        Value::UTinyInt(u8::MAX),
        Value::USmallInt(u16::MAX),
        Value::UInteger(u32::MAX),
        Value::UBigInt(u64::MAX),
        Value::UHugeInt(u128::MAX),
        Value::Float(0.1),
        Value::Float(f32::MIN),
        Value::Double(0.1),
        Value::Double(f64::MIN),
        Value::Decimal { unscaled: 1234, width: 6, scale: 2 },
        Value::Decimal { unscaled: -7, width: 18, scale: 6 },
        Value::Decimal { unscaled: i128::MIN + 1, width: 38, scale: 0 },
        Value::Varchar(String::new()),
        Value::Varchar("it's got a quote and a \\ backslash".to_string()),
        Value::Varchar("commas, brackets [] and braces {}".to_string()),
        Value::Blob(Vec::new()),
        Value::Blob(vec![0, 1, 0x7f, 0x80, 0xff]),
        Value::Date(19723),
        Value::Date(-1),
        Value::Time(86_399_999_999),
        Value::Timestamp(1_705_276_800_000_000),
        Value::Interval { months: -13, days: 2, micros: -3 },
        Value::List { element: LogicalType::Integer, values: Vec::new() },
        Value::List {
            element: LogicalType::Varchar,
            values: vec![Value::Varchar("a, b".to_string()), Value::Null],
        },
        Value::List {
            element: LogicalType::list(LogicalType::Integer),
            values: vec![Value::List {
                element: LogicalType::Integer,
                values: vec![Value::Integer(1), Value::Integer(2)],
            }],
        },
        Value::Struct(vec![
            ("a".to_string(), Value::Integer(1)),
            ("b b".to_string(), Value::Varchar("two".to_string())),
        ]),
    ];

    let mut exprs: Vec<ExprRef> = Vec::new();
    let mut labels: Vec<StrRef> = Vec::new();
    for (index, value) in constants.into_iter().enumerate() {
        exprs.push(plan.add_constant(value));
        labels.push(plan.intern(&format!("c{index}")));
    }

    // A typed null of every type, including the ones with no non-null representation yet.
    for (index, ty) in [
        LogicalType::Integer,
        LogicalType::Varchar,
        LogicalType::Uuid,
        LogicalType::Bit,
        LogicalType::TimeTz,
        LogicalType::TimestampTz,
        LogicalType::TimestampNs,
        LogicalType::map(LogicalType::Varchar, LogicalType::Integer),
        LogicalType::array(LogicalType::Double, 3),
    ]
    .into_iter()
    .enumerate()
    {
        let value = plan.add_value(Value::Null);
        exprs.push(plan.add_expr(Expr::Constant(value), ty));
        labels.push(plan.intern(&format!("n{index}")));
    }

    let exprs = plan.add_expr_list(&exprs);
    let names = plan.add_name_list(&labels);
    let project = plan.add_node(Node::Project { input: dummy, index: 0, exprs, names });
    plan.set_root(project);

    let dump = round_trips(&plan);
    assert!(!dump.contains("no textual form"), "a value has no printable form:\n{dump}");
    assert!(dump.contains("X'00017f80ff'::BLOB"), "the blob is not in\n{dump}");
    assert!(dump.contains("12.34::DECIMAL(6,2)"), "the decimal is not in\n{dump}");
    assert!(dump.contains("NULL::UUID"), "the typed null is not in\n{dump}");
}

/// The reader is the entry point for a plan someone wrote by hand, in a test fixture or a bug
/// report, so it has to accept text the printer would have produced without being handed the plan
/// that produced it first.
#[test]
fn a_dump_written_by_hand_reads_back() {
    let plan = reads_back(
        "\
Project #3 [#2.0::VARCHAR AS name, \"+\"(#2.1::BIGINT, 1::BIGINT)::BIGINT AS next]
  Filter ((#2.1::BIGINT >= 10::BIGINT)::BOOLEAN AND (#2.0::VARCHAR IS NOT DISTINCT FROM 'x'::VARCHAR)::BOOLEAN)::BOOLEAN
    Aggregate #2 groups=[#0.0::VARCHAR] aggregates=[sum(#1.0::BIGINT)::BIGINT]
      Join SEMI on=[(#0.0::VARCHAR = #1.1::VARCHAR)::BOOLEAN]
        Get memory.main.people AS p #0 [name::VARCHAR]
        Get memory.main.\"order counts\" AS o #1 [n::BIGINT, name::VARCHAR]
",
    );
    assert_eq!(plan.node_count(), 6, "one node per line");
    assert!(matches!(plan.node(plan.root()), Node::Project { .. }), "the root is the top line");
}

/// A plan is a tree and the indentation is the only thing that says which child is whose, so the
/// cases where the indentation is the whole difference get their own test.
#[test]
fn indentation_is_what_says_who_the_children_are() {
    let both_sides = reads_back(
        "\
CrossProduct
  Get memory.main.a AS a #0 [x::INTEGER]
  Get memory.main.b AS b #1 [y::INTEGER]
",
    );
    let Node::CrossProduct { left, right } = *both_sides.node(both_sides.root()) else {
        panic!("the root is a cross product");
    };
    assert!(matches!(both_sides.node(left), Node::Get { index: 0, .. }), "the left is a");
    assert!(matches!(both_sides.node(right), Node::Get { index: 1, .. }), "the right is b");

    let nested = reads_back(
        "\
CrossProduct
  CrossProduct
    Get memory.main.a AS a #0 [x::INTEGER]
    Get memory.main.b AS b #1 [y::INTEGER]
  Get memory.main.c AS c #2 [z::INTEGER]
",
    );
    let Node::CrossProduct { left, right } = *nested.node(nested.root()) else {
        panic!("the root is a cross product");
    };
    assert!(matches!(nested.node(left), Node::CrossProduct { .. }), "the left is the pair");
    assert!(matches!(nested.node(right), Node::Get { index: 2, .. }), "the right is c");
}

/// An aggregate and a scalar function print the same way, and the slot is what says which one it
/// is. `count_star()` in an aggregate list is an aggregate and the same text in a projection is a
/// call to a scalar function of that name. That is not a hole in the format: [`Plan::validate`]
/// refuses a plan with an aggregate anywhere but an aggregate list, so a plan with the other
/// reading never gets as far as being printed, and the round trip is still exact for every plan
/// that can exist.
#[test]
fn the_slot_is_what_says_an_aggregate_from_a_function() {
    let in_the_aggregate_list = reads_back(
        "\
Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]
  Get memory.main.a AS a #0 [x::INTEGER]
",
    );
    let Node::Aggregate { aggregates, .. } =
        *in_the_aggregate_list.node(in_the_aggregate_list.root())
    else {
        panic!("the root is an aggregate");
    };
    let counted = in_the_aggregate_list.expr_list(aggregates)[0];
    assert!(
        matches!(in_the_aggregate_list.expr(counted), Expr::Aggregate { .. }),
        "the aggregate list holds an aggregate"
    );

    let in_a_projection = reads_back(
        "\
Project #1 [count_star()::BIGINT AS c]
  Get memory.main.a AS a #0 [x::INTEGER]
",
    );
    let Node::Project { exprs, .. } = *in_a_projection.node(in_a_projection.root()) else {
        panic!("the root is a projection");
    };
    let called = in_a_projection.expr_list(exprs)[0];
    assert!(
        matches!(in_a_projection.expr(called), Expr::Function { .. }),
        "a projection holds a function call"
    );
}

/// The failure modes a person hits when they edit a dump by hand, all of which have to be errors
/// rather than a plan that is quietly a different plan.
#[test]
fn a_dump_that_is_wrong_says_so() {
    let broken = [
        (
            "a child under a leaf",
            "\
Get memory.main.a AS a #0 [x::INTEGER]
  Get memory.main.b AS b #1 [y::INTEGER]
",
        ),
        ("a missing child", "Filter TRUE::BOOLEAN\n"),
        (
            "a filter on an integer",
            "\
Filter 1::INTEGER
  Get memory.main.a AS a #0 [x::INTEGER]
",
        ),
        (
            "a decimal that does not have its own scale",
            "Project #0 [1.5::DECIMAL(6,2) AS d]\n  Dummy\n",
        ),
        (
            "an expression with no name in a projection",
            "Project #0 [1::INTEGER AS a, 2::INTEGER]\n  Dummy\n",
        ),
        ("a sort with no keys", "Sort []\n  Get memory.main.a AS a #0 [x::INTEGER]\n"),
        (
            "a conjunction of one thing",
            "\
Filter (#0.0::BOOLEAN)::BOOLEAN
  Get memory.main.a AS a #0 [x::BOOLEAN]
",
        ),
    ];
    for (what, text) in broken {
        assert!(Plan::parse(text).is_err(), "{what} was accepted:\n{text}");
    }
}
