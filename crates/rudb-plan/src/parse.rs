//! The reader for the textual form.
//!
//! Recursive descent over lines for the tree and over characters for the expressions. It is a
//! hand-written parser rather than a generated one because the grammar is fixed by the printer in
//! `print.rs`, the two are edited together, and the useful property is that every construct the
//! printer emits has exactly one place here that reads it back.
//!
//! Two things make this a small parser rather than a large one. Expressions are fully bracketed,
//! so there is no precedence and no lookahead beyond one token. And every expression is followed
//! by its type, so a constant is read with its type already in hand: `5` is an `INTEGER` or a
//! `DATE` or the unscaled part of a `DECIMAL(6,2)` depending on what comes after it, and none of
//! that has to be guessed from the digits.

use std::ops::Range;

use rudb_common::{Error, Field, LogicalType, Result, Value};

use crate::expr::{Arm, ColumnBinding, CompareOp, ConjunctionOp, Expr, SortKey};
use crate::node::{JoinKind, Node, SetOpKind};
use crate::plan::Plan;
use crate::{ExprRef, NodeRef, Slice};

impl Plan {
    /// Reads a plan back from its textual form.
    ///
    /// Printing the result produces the text that was read, which is a test in
    /// `tests/roundtrip.rs` rather than a claim here. The plan is validated before it is returned,
    /// so a text that parses is a text that names a plan somebody could have built.
    ///
    /// # Errors
    ///
    /// With the line number and the column, because the thing a person wants from a dump that will
    /// not read back is which character of which operator.
    pub fn parse(text: &str) -> Result<Self> {
        let lines = split_lines(text)?;
        if lines.is_empty() {
            return Err(Error::parser("a plan has at least one operator".to_string()));
        }
        let mut reader = Reader { lines, at: 0, plan: Self::without_nodes() };
        let root = reader.node(0)?;
        if let Some(line) = reader.lines.get(reader.at) {
            return Err(Error::parser(format!(
                "line {}: \"{}\" is past the end of the plan",
                line.number, line.text
            )));
        }
        let mut plan = reader.plan;
        plan.set_root(root);
        plan.validate()?;
        Ok(plan)
    }
}

/// One operator line, with its blank lines and its indentation already dealt with.
#[derive(Debug)]
struct Line<'a> {
    /// How many levels in, which is half the leading spaces.
    depth: usize,
    /// The line with the indentation removed.
    text: &'a str,
    /// The line number in the original text, one based, for error messages.
    number: usize,
}

fn split_lines(text: &str) -> Result<Vec<Line<'_>>> {
    let mut lines = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if raw.contains('\t') {
            return Err(Error::parser(format!("line {number}: indented with a tab")));
        }
        let spaces = raw.len() - raw.trim_start_matches(' ').len();
        if spaces % 2 != 0 {
            return Err(Error::parser(format!(
                "line {number}: indented {spaces} spaces, which is not a whole number of levels"
            )));
        }
        lines.push(Line { depth: spaces / 2, text: raw[spaces..].trim_end(), number });
    }
    Ok(lines)
}

struct Reader<'a> {
    lines: Vec<Line<'a>>,
    at: usize,
    plan: Plan,
}

impl Reader<'_> {
    /// Reads the operator at `depth` and everything under it.
    ///
    /// Children are read before the node is built, which is not a stylistic choice: the arena's
    /// backwards-reference rule says a child index is smaller than its parent's, and building the
    /// parent first would break it on every plan with more than one operator.
    fn node(&mut self, depth: usize) -> Result<NodeRef> {
        let Some(line) = self.lines.get(self.at) else {
            return Err(Error::parser(format!("expected an operator {depth} levels in")));
        };
        if line.depth != depth {
            return Err(Error::parser(format!(
                "line {}: \"{}\" is {} levels in and {depth} was expected",
                line.number, line.text, line.depth
            )));
        }
        let number = line.number;
        let text = line.text;
        self.at += 1;

        let (keyword, arguments) = match text.find(' ') {
            Some(space) => (&text[..space], &text[space..]),
            None => (text, ""),
        };
        let mut cursor = Cursor::new(arguments);
        let built = self
            .arguments(keyword, &mut cursor)
            .map_err(|error| Error::parser(format!("line {number}: {}", error.message())))?;
        if !cursor.done() {
            return Err(Error::parser(format!(
                "line {number}: \"{}\" is left over after the {keyword}",
                cursor.rest()
            )));
        }

        let left = if built.arity > 0 { Some(self.node(depth + 1)?) } else { None };
        let right = if built.arity > 1 { Some(self.node(depth + 1)?) } else { None };
        Ok(self.plan.add_node((built.assemble)(left.unwrap_or(0), right.unwrap_or(0))))
    }

    /// Reads one operator's arguments, returning how many inputs it takes and how to build it once
    /// they have been read.
    fn arguments(&mut self, keyword: &str, c: &mut Cursor<'_>) -> Result<Built> {
        let plan = &mut self.plan;
        match keyword {
            "Dummy" => Ok(Built::leaf(Node::Dummy)),
            "CrossProduct" => Ok(Built {
                arity: 2,
                assemble: Box::new(|left, right| Node::CrossProduct { left, right }),
            }),
            "Get" => {
                let catalog = read_name(plan, c)?;
                c.expect(".")?;
                let schema = read_name(plan, c)?;
                c.expect(".")?;
                let table = read_name(plan, c)?;
                c.expect_word("AS")?;
                let alias = read_name(plan, c)?;
                let index = read_table_index(c)?;
                let columns = read_schema(plan, c)?;
                Ok(Built::leaf(Node::Get { catalog, schema, table, alias, index, columns }))
            }
            "Values" => {
                let index = read_table_index(c)?;
                let columns = read_schema(plan, c)?;
                c.expect_word("rows")?;
                c.expect("=")?;
                c.expect("[")?;
                let mut rows = Vec::new();
                if !c.eat_space_then("]") {
                    loop {
                        rows.push(read_expr_list(plan, c)?);
                        if !c.eat_space_then(",") {
                            break;
                        }
                    }
                    c.expect("]")?;
                }
                let rows = plan.add_rows(&rows);
                Ok(Built::leaf(Node::Values { index, columns, rows }))
            }
            "Filter" => {
                let predicate = read_expr(plan, c)?;
                Ok(Built::unary(move |input| Node::Filter { input, predicate }))
            }
            "Project" => {
                let index = read_table_index(c)?;
                let mut exprs = Vec::new();
                let mut names = Vec::new();
                c.expect("[")?;
                if !c.eat_space_then("]") {
                    loop {
                        exprs.push(read_expr(plan, c)?);
                        c.expect_word("AS")?;
                        names.push(read_name(plan, c)?);
                        if !c.eat_space_then(",") {
                            break;
                        }
                    }
                    c.expect("]")?;
                }
                let exprs = plan.add_expr_list(&exprs);
                let names = plan.add_name_list(&names);
                Ok(Built::unary(move |input| Node::Project { input, index, exprs, names }))
            }
            "Aggregate" => {
                let index = read_table_index(c)?;
                c.expect_word("groups")?;
                c.expect("=")?;
                let groups = read_expr_list(plan, c)?;
                c.expect_word("aggregates")?;
                c.expect("=")?;
                let aggregates = read_aggregate_list(plan, c)?;
                Ok(Built::unary(move |input| Node::Aggregate { input, index, groups, aggregates }))
            }
            "Sort" => {
                let mut keys = Vec::new();
                c.expect("[")?;
                if !c.eat_space_then("]") {
                    loop {
                        keys.push(read_sort_key(plan, c)?);
                        if !c.eat_space_then(",") {
                            break;
                        }
                    }
                    c.expect("]")?;
                }
                let keys = plan.add_sort_keys(&keys);
                Ok(Built::unary(move |input| Node::Sort { input, keys }))
            }
            "Limit" => {
                let count = if c.eat_word("ALL") { None } else { Some(read_count(c)?) };
                c.expect_word("offset")?;
                let offset = read_count(c)?;
                Ok(Built::unary(move |input| Node::Limit { input, count, offset }))
            }
            "Distinct" => {
                c.expect_word("on")?;
                c.expect("=")?;
                let on = read_expr_list(plan, c)?;
                Ok(Built::unary(move |input| Node::Distinct { input, on }))
            }
            "Join" => {
                let kind = read_keyword(c, &JoinKind::ALL, JoinKind::keyword, "a join kind")?;
                c.expect_word("on")?;
                c.expect("=")?;
                let conditions = read_expr_list(plan, c)?;
                Ok(Built {
                    arity: 2,
                    assemble: Box::new(move |left, right| Node::Join {
                        left,
                        right,
                        kind,
                        conditions,
                    }),
                })
            }
            "SetOp" => {
                let kind = read_keyword(c, &SetOpKind::ALL, SetOpKind::keyword, "a set operation")?;
                let all = if c.eat_word("ALL") {
                    true
                } else if c.eat_word("DISTINCT") {
                    false
                } else {
                    return Err(c.error("expected ALL or DISTINCT"));
                };
                let index = read_table_index(c)?;
                Ok(Built {
                    arity: 2,
                    assemble: Box::new(move |left, right| Node::SetOp {
                        left,
                        right,
                        kind,
                        all,
                        index,
                    }),
                })
            }
            other => Err(Error::parser(format!("\"{other}\" is not an operator"))),
        }
    }
}

/// An operator whose arguments have been read and whose inputs have not.
struct Built {
    arity: usize,
    assemble: Box<dyn FnOnce(NodeRef, NodeRef) -> Node>,
}

impl Built {
    fn leaf(node: Node) -> Self {
        Self { arity: 0, assemble: Box::new(move |_, _| node) }
    }

    fn unary(make: impl FnOnce(NodeRef) -> Node + 'static) -> Self {
        Self { arity: 1, assemble: Box::new(move |input, _| make(input)) }
    }
}

// Node arguments.

fn read_table_index(c: &mut Cursor<'_>) -> Result<u32> {
    c.expect("#")?;
    read_number(c)
}

fn read_count(c: &mut Cursor<'_>) -> Result<u64> {
    c.skip_space();
    let start = c.at;
    while c.peek().is_some_and(|ch| ch.is_ascii_digit()) {
        c.at += 1;
    }
    if c.at == start {
        return Err(c.error("expected a row count"));
    }
    c.text[start..c.at].parse().map_err(|_| c.error("that row count does not fit in 64 bits"))
}

fn read_number(c: &mut Cursor<'_>) -> Result<u32> {
    c.skip_space();
    let start = c.at;
    while c.peek().is_some_and(|ch| ch.is_ascii_digit()) {
        c.at += 1;
    }
    if c.at == start {
        return Err(c.error("expected a number"));
    }
    c.text[start..c.at].parse().map_err(|_| c.error("that number does not fit in 32 bits"))
}

/// One of a fixed set of keywords, matched longest first so that a keyword which is a prefix of
/// another cannot shadow it.
fn read_keyword<T: Copy>(
    c: &mut Cursor<'_>,
    all: &[T],
    spell: impl Fn(T) -> &'static str,
    what: &str,
) -> Result<T> {
    let mut candidates: Vec<T> = all.to_vec();
    candidates.sort_by_key(|&kind| std::cmp::Reverse(spell(kind).len()));
    for candidate in candidates {
        if c.eat_word(spell(candidate)) {
            return Ok(candidate);
        }
    }
    Err(c.error(&format!("expected {what}")))
}

fn read_name(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<u32> {
    let name = read_identifier(c)?;
    Ok(plan.intern(&name))
}

/// A named and typed column list, which is what a scan and a `VALUES` produce.
fn read_schema(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<Slice> {
    let mut fields = Vec::new();
    c.expect("[")?;
    if !c.eat_space_then("]") {
        loop {
            let name = read_identifier(c)?;
            c.expect("::")?;
            let ty = read_type(c)?;
            fields.push(Field::new(name, ty));
            if !c.eat_space_then(",") {
                break;
            }
        }
        c.expect("]")?;
    }
    Ok(plan.add_fields(&fields))
}

fn read_expr_list(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<Slice> {
    let mut exprs = Vec::new();
    c.expect("[")?;
    if !c.eat_space_then("]") {
        loop {
            exprs.push(read_expr(plan, c)?);
            if !c.eat_space_then(",") {
                break;
            }
        }
        c.expect("]")?;
    }
    Ok(plan.add_expr_list(&exprs))
}

/// The aggregate list of an `Aggregate`, which is the only place an aggregate can appear.
///
/// An aggregate and a scalar function print identically, so this is not a different syntax, it is
/// the same syntax read in the one slot where it means something else. Reading it anywhere else
/// would produce a plan `Plan::validate` rejects, which is the check that keeps the two in step.
fn read_aggregate_list(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<Slice> {
    let mut exprs = Vec::new();
    c.expect("[")?;
    if !c.eat_space_then("]") {
        loop {
            exprs.push(read_aggregate(plan, c)?);
            if !c.eat_space_then(",") {
                break;
            }
        }
        c.expect("]")?;
    }
    Ok(plan.add_expr_list(&exprs))
}

fn read_aggregate(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<ExprRef> {
    c.skip_space();
    let Some(name) = try_call_name(c) else {
        return Err(c.error("expected an aggregate call"));
    };
    let name = plan.intern(&name);
    let distinct = c.eat_keyword_before_argument("DISTINCT");
    let mut args = Vec::new();
    let mut filter = None;
    if !c.eat_space_then(")") {
        // `count(*) FILTER (WHERE p)` binds to a zero argument aggregate with a filter, so the
        // filter has to be reachable without going through the argument loop first.
        let mut filtered = c.eat_keyword_before_argument("FILTER");
        if !filtered {
            loop {
                args.push(read_expr(plan, c)?);
                if !c.eat_space_then(",") {
                    break;
                }
            }
            filtered = c.eat_keyword_before_argument("FILTER");
        }
        if filtered {
            filter = Some(read_expr(plan, c)?);
        }
        c.expect(")")?;
    }
    let args = plan.add_expr_list(&args);
    let ty = read_annotation(c)?;
    Ok(plan.add_expr(Expr::Aggregate { name, args, distinct, filter }, ty))
}

fn read_sort_key(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<SortKey> {
    let expr = read_expr(plan, c)?;
    let descending = if c.eat_word("DESC") {
        true
    } else if c.eat_word("ASC") {
        false
    } else {
        return Err(c.error("expected ASC or DESC"));
    };
    c.expect_word("NULLS")?;
    let nulls_first = if c.eat_word("FIRST") {
        true
    } else if c.eat_word("LAST") {
        false
    } else {
        return Err(c.error("expected FIRST or LAST"));
    };
    Ok(SortKey { expr, descending, nulls_first })
}

// Expressions.

/// What an expression's form turned out to be.
///
/// A constant cannot be finished until its type has been read, because the type is what says
/// whether `5` is an integer, a day number or the unscaled part of a decimal. Everything else is
/// finished by the time the type arrives.
enum Form {
    Done(Expr),
    Constant(Range<usize>),
}

fn read_expr(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<ExprRef> {
    let form = read_form(plan, c)?;
    let ty = read_annotation(c)?;
    let expr = match form {
        Form::Done(expr) => expr,
        Form::Constant(span) => {
            let text = &c.text[span];
            let value = read_value(text, &ty)
                .map_err(|error| c.error(&format!("{text} is not a {ty}: {}", error.message())))?;
            Expr::Constant(plan.add_value(value))
        }
    };
    Ok(plan.add_expr(expr, ty))
}

fn read_annotation(c: &mut Cursor<'_>) -> Result<LogicalType> {
    c.expect("::")?;
    read_type(c)
}

fn read_type(c: &mut Cursor<'_>) -> Result<LogicalType> {
    c.skip_space();
    let end = type_extent(c.text, c.at);
    if end == c.at {
        return Err(c.error("expected a type"));
    }
    let text = &c.text[c.at..end];
    c.at = end;
    LogicalType::parse(text)
}

fn read_form(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<Form> {
    c.skip_space();
    if c.eat("#") {
        let table = read_number(c)?;
        c.expect(".")?;
        let column = read_number(c)?;
        return Ok(Form::Done(Expr::Column(ColumnBinding::new(table, column))));
    }
    if c.eat("(") {
        return read_bracketed(plan, c).map(Form::Done);
    }
    if c.eat_word("CASE") {
        return read_case(plan, c).map(Form::Done);
    }
    for (word, try_cast) in [("TRY_CAST", true), ("CAST", false)] {
        if c.eat_word(word) {
            c.expect("(")?;
            let input = read_expr(plan, c)?;
            c.expect(")")?;
            return Ok(Form::Done(Expr::Cast { input, try_cast }));
        }
    }
    if let Some(name) = try_call_name(c) {
        let name = plan.intern(&name);
        let mut args = Vec::new();
        if !c.eat_space_then(")") {
            loop {
                args.push(read_expr(plan, c)?);
                if !c.eat_space_then(",") {
                    break;
                }
            }
            c.expect(")")?;
        }
        let args = plan.add_expr_list(&args);
        return Ok(Form::Done(Expr::Function { name, args }));
    }

    let end = literal_extent(c.text, c.at);
    if end == c.at {
        return Err(c.error("expected an expression"));
    }
    let span = c.at..end;
    c.at = end;
    Ok(Form::Constant(span))
}

/// A comparison or a conjunction, with the opening parenthesis already eaten.
///
/// Which of the two it is only becomes clear after the first operand, since both start the same
/// way. That is one token of lookahead over an operand that has already been parsed, which is why
/// there is no backtracking here.
fn read_bracketed(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<Expr> {
    let left = read_expr(plan, c)?;
    for op in [ConjunctionOp::And, ConjunctionOp::Or] {
        if c.eat_word(op.keyword()) {
            let mut children = vec![left];
            loop {
                children.push(read_expr(plan, c)?);
                if !c.eat_word(op.keyword()) {
                    break;
                }
            }
            c.expect(")")?;
            let children = plan.add_expr_list(&children);
            return Ok(Expr::Conjunction { op, children });
        }
    }
    c.skip_space();
    let found = CompareOp::SPELLINGS.into_iter().find(|op| c.eat(op.symbol()));
    let Some(op) = found else {
        return Err(c.error("expected a comparison, AND or OR"));
    };
    let right = read_expr(plan, c)?;
    c.expect(")")?;
    Ok(Expr::Compare { op, left, right })
}

fn read_case(plan: &mut Plan, c: &mut Cursor<'_>) -> Result<Expr> {
    let mut arms = Vec::new();
    while c.eat_word("WHEN") {
        let when = read_expr(plan, c)?;
        c.expect_word("THEN")?;
        let then = read_expr(plan, c)?;
        arms.push(Arm { when, then });
    }
    let otherwise = if c.eat_word("ELSE") { Some(read_expr(plan, c)?) } else { None };
    c.expect_word("END")?;
    let arms = plan.add_arms(&arms);
    Ok(Expr::Case { arms, otherwise })
}

/// A function name followed immediately by its opening parenthesis, or nothing consumed.
///
/// The parenthesis has to be adjacent, which is what separates a call from the bare words `NULL`,
/// `TRUE` and `FALSE`. It is also why a function called `cast` is printed quoted: an unquoted
/// `CAST(` is syntax and is checked before this runs, and `"cast"(` reaches here.
fn try_call_name(c: &mut Cursor<'_>) -> Option<String> {
    let start = c.at;
    let Ok(name) = read_identifier(c) else {
        c.at = start;
        return None;
    };
    if c.peek() == Some('(') {
        c.at += 1;
        Some(name)
    } else {
        c.at = start;
        None
    }
}

fn read_identifier(c: &mut Cursor<'_>) -> Result<String> {
    c.skip_space();
    if c.eat("\"") {
        let mut name = String::new();
        loop {
            let Some(character) = c.peek() else {
                return Err(c.error("a quoted name is not closed"));
            };
            c.at += character.len_utf8();
            if character == '"' {
                if c.peek() == Some('"') {
                    name.push('"');
                    c.at += 1;
                    continue;
                }
                break;
            }
            name.push(character);
        }
        return Ok(name);
    }
    let start = c.at;
    while c.peek().is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        c.at += 1;
    }
    if c.at == start {
        return Err(c.error("expected a name"));
    }
    Ok(c.text[start..c.at].to_string())
}

// Constants, read with their type already known.

fn read_value(text: &str, ty: &LogicalType) -> Result<Value> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("NULL") {
        return Ok(Value::Null);
    }
    let whole = |what: &str| Error::parser(format!("{text} is not {what}"));
    match ty {
        LogicalType::Boolean => match text {
            "TRUE" => Ok(Value::Boolean(true)),
            "FALSE" => Ok(Value::Boolean(false)),
            _ => Err(whole("TRUE or FALSE")),
        },
        LogicalType::TinyInt => text.parse().map(Value::TinyInt).map_err(|_| whole("a TINYINT")),
        LogicalType::SmallInt => text.parse().map(Value::SmallInt).map_err(|_| whole("a SMALLINT")),
        LogicalType::Integer => text.parse().map(Value::Integer).map_err(|_| whole("an INTEGER")),
        LogicalType::BigInt => text.parse().map(Value::BigInt).map_err(|_| whole("a BIGINT")),
        LogicalType::HugeInt => text.parse().map(Value::HugeInt).map_err(|_| whole("a HUGEINT")),
        LogicalType::UTinyInt => text.parse().map(Value::UTinyInt).map_err(|_| whole("a UTINYINT")),
        LogicalType::USmallInt => {
            text.parse().map(Value::USmallInt).map_err(|_| whole("a USMALLINT"))
        }
        LogicalType::UInteger => text.parse().map(Value::UInteger).map_err(|_| whole("a UINTEGER")),
        LogicalType::UBigInt => text.parse().map(Value::UBigInt).map_err(|_| whole("a UBIGINT")),
        LogicalType::UHugeInt => text.parse().map(Value::UHugeInt).map_err(|_| whole("a UHUGEINT")),
        LogicalType::Float => text.parse().map(Value::Float).map_err(|_| whole("a FLOAT")),
        LogicalType::Double => text.parse().map(Value::Double).map_err(|_| whole("a DOUBLE")),
        LogicalType::Decimal { width, scale } => read_decimal(text, *width, *scale),
        LogicalType::Varchar => read_string(text).map(Value::Varchar),
        LogicalType::Blob => read_blob(text).map(Value::Blob),
        LogicalType::Date => text.parse().map(Value::Date).map_err(|_| whole("a day number")),
        LogicalType::Time => {
            text.parse().map(Value::Time).map_err(|_| whole("a microsecond count"))
        }
        LogicalType::Timestamp => {
            text.parse().map(Value::Timestamp).map_err(|_| whole("a microsecond count"))
        }
        LogicalType::Interval => {
            let parts = read_braced(text)?;
            let [months, days, micros] = parts.as_slice() else {
                return Err(whole("an interval, which is three numbers in braces"));
            };
            Ok(Value::Interval {
                months: months.trim().parse().map_err(|_| whole("an interval"))?,
                days: days.trim().parse().map_err(|_| whole("an interval"))?,
                micros: micros.trim().parse().map_err(|_| whole("an interval"))?,
            })
        }
        LogicalType::List(element) => {
            let values = read_braced(text)?
                .into_iter()
                .map(|part| read_value(part, element))
                .collect::<Result<Vec<_>>>()?;
            Ok(Value::List { element: element.as_ref().clone(), values })
        }
        LogicalType::Struct(fields) => {
            let parts = read_braced(text)?;
            if parts.len() != fields.len() {
                return Err(whole(&format!("a struct of {} fields", fields.len())));
            }
            let mut held = Vec::with_capacity(parts.len());
            for (part, field) in parts.into_iter().zip(fields) {
                held.push((field.name.clone(), read_value(part, &field.ty)?));
            }
            Ok(Value::Struct(held))
        }
        // Everything left is a type rudb_common::Value cannot hold, so a constant of that type
        // cannot have been printed and cannot be built here either. A typed null of one of them is
        // fine and was handled above.
        other => Err(Error::not_implemented(format!(
            "a constant of type {other} has no value representation yet"
        ))),
    }
}

fn read_decimal(text: &str, width: u8, scale: u8) -> Result<Value> {
    let bad = || Error::parser(format!("{text} is not a DECIMAL({width},{scale})"));
    let negative = text.starts_with('-');
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let digits = if scale == 0 {
        if body.contains('.') {
            return Err(bad());
        }
        body.to_string()
    } else {
        let (whole, fraction) = body.split_once('.').ok_or_else(bad)?;
        if fraction.len() != usize::from(scale) {
            return Err(bad());
        }
        format!("{whole}{fraction}")
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad());
    }
    let signed = if negative { format!("-{digits}") } else { digits };
    let unscaled: i128 = signed.parse().map_err(|_| bad())?;
    Ok(Value::Decimal { unscaled, width, scale })
}

fn read_string(text: &str) -> Result<String> {
    let inner = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .ok_or_else(|| Error::parser(format!("{text} is not a quoted string")))?;
    let mut out = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                if characters.next() != Some('\'') {
                    return Err(Error::parser(format!("{text} has a quote that is not doubled")));
                }
                out.push('\'');
            }
            '\\' => match characters.next() {
                Some('\\') => out.push('\\'),
                Some('x') => {
                    let high = characters.next().unwrap_or(' ');
                    let low = characters.next().unwrap_or(' ');
                    let code = u8::from_str_radix(&format!("{high}{low}"), 16)
                        .map_err(|_| Error::parser(format!("{text} has a bad escape")))?;
                    out.push(char::from(code));
                }
                _ => return Err(Error::parser(format!("{text} has a bad escape"))),
            },
            other => out.push(other),
        }
    }
    Ok(out)
}

fn read_blob(text: &str) -> Result<Vec<u8>> {
    let bad = || Error::parser(format!("{text} is not a hex blob"));
    let inner = text
        .strip_prefix(['X', 'x'])
        .and_then(|rest| rest.strip_prefix('\''))
        .and_then(|rest| rest.strip_suffix('\''))
        .ok_or_else(bad)?;
    if inner.len() % 2 != 0 {
        return Err(bad());
    }
    let mut bytes = Vec::with_capacity(inner.len() / 2);
    for pair in inner.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(pair).map_err(|_| bad())?;
        bytes.push(u8::from_str_radix(pair, 16).map_err(|_| bad())?);
    }
    Ok(bytes)
}

/// The comma separated parts of a braced list, at brace depth one.
fn read_braced(text: &str) -> Result<Vec<&str>> {
    let inner = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| Error::parser(format!("{text} is not a braced list")))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = inner.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => at = skip_string(inner, at) - 1,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(inner[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    parts.push(inner[start..].trim());
    Ok(parts)
}

// Extents. Both of these answer the same question from two directions: how much of this text
// belongs to the thing that starts here. They are byte scans rather than parses because the answer
// is handed to a real parser afterwards, and a second grammar that has to agree with the first is
// a second grammar that will not.

/// How far a type annotation starting at `from` runs.
///
/// A type is a name, then an optional balanced parenthesis group, then optionally `WITH TIME ZONE`,
/// then any number of balanced bracket groups. The time zone suffix comes before the brackets and
/// not after, because a list of them prints as `TIME WITH TIME ZONE[]`: the suffix belongs to the
/// element type and the brackets are the list wrapped around it. Every type
/// [`LogicalType`](rudb_common::LogicalType) prints fits that, which `print::prints_readably`
/// asserts over the whole type set.
pub(crate) fn type_extent(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let mut at = if bytes.get(from) == Some(&b'"') {
        skip_quoted_name(text, from)
    } else {
        let mut at = from;
        while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
            at += 1;
        }
        at
    };
    at = balanced(text, at, b'(', b')');
    for suffix in [" WITH TIME ZONE", " WITHOUT TIME ZONE"] {
        if starts_with_ignoring_case(&text[at..], suffix) {
            at += suffix.len();
            break;
        }
    }
    loop {
        let next = balanced(text, at, b'[', b']');
        if next == at {
            break;
        }
        at = next;
    }
    at
}

/// How far a constant starting at `from` runs, which is up to the `::` that types it.
///
/// Braces nest and single quotes hide everything inside them, so a string holding a colon pair and
/// a list of structs both come out in one piece.
fn literal_extent(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    let mut at = from;
    let mut depth = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => {
                at = skip_string(text, at);
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => return at,
            _ => {}
        }
        at += 1;
    }
    at
}

/// Past a balanced group starting at `at`, or `at` unchanged if no group starts there.
fn balanced(text: &str, at: usize, open: u8, close: u8) -> usize {
    let bytes = text.as_bytes();
    if bytes.get(at) != Some(&open) {
        return at;
    }
    let mut depth = 0usize;
    let mut here = at;
    while here < bytes.len() {
        match bytes[here] {
            b'"' => {
                here = skip_quoted_name(text, here);
                continue;
            }
            b'\'' => {
                here = skip_string(text, here);
                continue;
            }
            byte if byte == open => depth += 1,
            byte if byte == close => {
                depth -= 1;
                if depth == 0 {
                    return here + 1;
                }
            }
            _ => {}
        }
        here += 1;
    }
    // Unbalanced. Handing the rest of the line to the type parser gets a message naming the text
    // that is wrong, which is more useful than one naming the character where counting stopped.
    text.len()
}

/// Past a single quoted string starting at `at`, doubled quotes included.
fn skip_string(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    let mut here = at + 1;
    while here < bytes.len() {
        if bytes[here] == b'\'' {
            if bytes.get(here + 1) == Some(&b'\'') {
                here += 2;
                continue;
            }
            return here + 1;
        }
        here += 1;
    }
    text.len()
}

/// Past a double quoted name starting at `at`, doubled quotes included.
fn skip_quoted_name(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    let mut here = at + 1;
    while here < bytes.len() {
        if bytes[here] == b'"' {
            if bytes.get(here + 1) == Some(&b'"') {
                here += 2;
                continue;
            }
            return here + 1;
        }
        here += 1;
    }
    text.len()
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// A position in one operator's argument text.
struct Cursor<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn skip_space(&mut self) {
        while self.rest().starts_with(' ') {
            self.at += 1;
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        if self.rest().starts_with(token) {
            self.at += token.len();
            true
        } else {
            false
        }
    }

    /// Eats a keyword, case insensitively, only if a whole word is there.
    ///
    /// Without the boundary check `ALL` would match the front of a column called `ALLOWED` and the
    /// error would land on whatever came after it.
    fn eat_word(&mut self, word: &str) -> bool {
        let start = self.at;
        self.skip_space();
        if !starts_with_ignoring_case(self.rest(), word) {
            self.at = start;
            return false;
        }
        let after = self.text[self.at + word.len()..].chars().next();
        if after.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            self.at = start;
            return false;
        }
        self.at += word.len();
        true
    }

    /// Eats a keyword that stands where an argument could stand.
    ///
    /// `DISTINCT` and `FILTER` sit just inside an aggregate's parentheses, which is exactly where a
    /// call to a function of the same name could be, and [`Cursor::eat_word`] would take
    /// `filter(x)` for the keyword followed by a parenthesised expression. The printed form always
    /// puts a space after these two and a call always has its parenthesis hard against the name,
    /// so requiring the space is what tells them apart.
    fn eat_keyword_before_argument(&mut self, word: &str) -> bool {
        let start = self.at;
        self.skip_space();
        if starts_with_ignoring_case(self.rest(), word)
            && self.text[self.at + word.len()..].starts_with(' ')
        {
            self.at += word.len();
            return true;
        }
        self.at = start;
        false
    }

    /// Eats a token after any spaces, leaving the cursor alone if the token is not there.
    fn eat_space_then(&mut self, token: &str) -> bool {
        let start = self.at;
        self.skip_space();
        if self.eat(token) {
            true
        } else {
            self.at = start;
            false
        }
    }

    fn expect(&mut self, token: &str) -> Result<()> {
        if self.eat_space_then(token) {
            Ok(())
        } else {
            Err(self.error(&format!("expected \"{token}\"")))
        }
    }

    fn expect_word(&mut self, word: &str) -> Result<()> {
        if self.eat_word(word) { Ok(()) } else { Err(self.error(&format!("expected \"{word}\""))) }
    }

    fn done(&mut self) -> bool {
        self.skip_space();
        self.at >= self.text.len()
    }

    fn error(&self, what: &str) -> Error {
        Error::parser(format!("{what} at column {}, reading \"{}\"", self.at + 1, self.text.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::{RESERVED, is_plain_identifier};

    #[test]
    fn a_type_annotation_ends_where_the_type_does() {
        for (text, expected) in [
            ("INTEGER DESC", "INTEGER"),
            ("INTEGER, x", "INTEGER"),
            ("INTEGER)", "INTEGER"),
            ("DECIMAL(18,3) AS a", "DECIMAL(18,3)"),
            ("INTEGER[] AS a", "INTEGER[]"),
            ("INTEGER[3][] ", "INTEGER[3][]"),
            ("MAP(VARCHAR, INTEGER)[]", "MAP(VARCHAR, INTEGER)[]"),
            ("STRUCT(a INTEGER, b VARCHAR) AS s", "STRUCT(a INTEGER, b VARCHAR)"),
            ("TIMESTAMP WITH TIME ZONE, x", "TIMESTAMP WITH TIME ZONE"),
            ("TIME WITH TIME ZONE]", "TIME WITH TIME ZONE"),
            ("TIME WITH TIME ZONE[], x", "TIME WITH TIME ZONE[]"),
            ("TIMESTAMP WITH TIME ZONE[4] AS t", "TIMESTAMP WITH TIME ZONE[4]"),
            ("TIMESTAMP DESC", "TIMESTAMP"),
            ("\"NULL\" AS n", "\"NULL\""),
        ] {
            let end = type_extent(text, 0);
            assert_eq!(&text[..end], expected, "wrong extent in {text}");
            LogicalType::parse(expected)
                .unwrap_or_else(|e| panic!("{expected} does not parse: {e}"));
        }
    }

    /// A field name inside a struct that needs quoting can hold a bracket, and the extent scanner
    /// counting it would end the type in the middle of itself.
    #[test]
    fn a_quoted_field_name_does_not_end_the_type_early() {
        let text = "STRUCT(\"a)b\" INTEGER) AS s";
        let end = type_extent(text, 0);
        assert_eq!(&text[..end], "STRUCT(\"a)b\" INTEGER)");
    }

    #[test]
    fn a_constant_ends_at_the_colon_pair_that_types_it() {
        for (text, expected) in [
            ("5::INTEGER", "5"),
            ("-5::INTEGER", "-5"),
            ("''::VARCHAR", "''"),
            ("'a::b'::VARCHAR", "'a::b'"),
            ("'it''s'::VARCHAR", "'it''s'"),
            ("{1, 2}::INTEGER[]", "{1, 2}"),
            ("{{1}, {2}}::INTEGER[][]", "{{1}, {2}}"),
            ("X'00ff'::BLOB", "X'00ff'"),
            ("NULL::VARCHAR", "NULL"),
        ] {
            let end = literal_extent(text, 0);
            assert_eq!(&text[..end], expected, "wrong extent in {text}");
        }
    }

    #[test]
    fn a_decimal_reads_back_at_its_own_scale() {
        assert_eq!(
            read_decimal("12.34", 6, 2).unwrap(),
            Value::Decimal { unscaled: 1234, width: 6, scale: 2 }
        );
        assert_eq!(
            read_decimal("-0.005", 6, 3).unwrap(),
            Value::Decimal { unscaled: -5, width: 6, scale: 3 }
        );
        assert_eq!(
            read_decimal("1234", 6, 0).unwrap(),
            Value::Decimal { unscaled: 1234, width: 6, scale: 0 }
        );
    }

    /// The digits after the point have to be exactly the scale. `1.5::DECIMAL(6,2)` is a hundred
    /// times off from `1.50::DECIMAL(6,2)` and there is no reading of it that is obviously right,
    /// so it is rejected rather than guessed at.
    #[test]
    fn a_decimal_with_the_wrong_number_of_digits_is_rejected() {
        assert!(read_decimal("1.5", 6, 2).is_err());
        assert!(read_decimal("1.500", 6, 2).is_err());
        assert!(read_decimal("15", 6, 2).is_err());
        assert!(read_decimal("1.5", 6, 0).is_err());
    }

    #[test]
    fn a_string_gives_its_quotes_and_escapes_back() {
        assert_eq!(read_string("''").unwrap(), "");
        assert_eq!(read_string("'it''s'").unwrap(), "it's");
        assert_eq!(read_string("'a\\\\b'").unwrap(), "a\\b");
        assert_eq!(read_string("'one\\x0atwo'").unwrap(), "one\ntwo");
        assert!(read_string("'").is_err());
        assert!(read_string("no quotes").is_err());
    }

    #[test]
    fn a_blob_is_pairs_of_hex_digits() {
        assert_eq!(read_blob("X''").unwrap(), Vec::<u8>::new());
        assert_eq!(read_blob("X'00ff10'").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(read_blob("X'0'").is_err(), "an odd number of digits is not a byte string");
        assert!(read_blob("X'zz'").is_err());
    }

    #[test]
    fn a_braced_list_splits_at_the_top_level_only() {
        assert_eq!(read_braced("{}").unwrap(), Vec::<&str>::new());
        assert_eq!(read_braced("{1}").unwrap(), vec!["1"]);
        assert_eq!(read_braced("{1, 2}").unwrap(), vec!["1", "2"]);
        assert_eq!(read_braced("{{1, 2}, {3}}").unwrap(), vec!["{1, 2}", "{3}"]);
        assert_eq!(read_braced("{'a,b', 'c'}").unwrap(), vec!["'a,b'", "'c'"]);
        assert!(read_braced("1, 2").is_err());
    }

    #[test]
    fn a_keyword_needs_a_word_boundary() {
        let mut c = Cursor::new(" ALLOWED");
        assert!(!c.eat_word("ALL"), "ALL should not match the front of ALLOWED");
        assert!(c.eat_word("ALLOWED"));
        let mut c = Cursor::new(" all ");
        assert!(c.eat_word("ALL"), "keywords are case insensitive");
    }

    #[test]
    fn an_odd_indent_is_an_error_and_says_so() {
        let message = Plan::parse("Filter x\n   Dummy\n").unwrap_err().to_string();
        assert!(message.contains("whole number of levels"), "unhelpful message: {message}");
    }

    #[test]
    fn a_tab_is_an_error_rather_than_a_guess() {
        let message = Plan::parse("Filter x\n\tDummy\n").unwrap_err().to_string();
        assert!(message.contains("tab"), "unhelpful message: {message}");
    }

    #[test]
    fn an_unknown_operator_names_itself() {
        let message = Plan::parse("Frobnicate\n").unwrap_err().to_string();
        assert!(message.contains("Frobnicate"), "unhelpful message: {message}");
    }

    #[test]
    fn a_missing_child_is_an_error_rather_than_a_plan_with_a_hole() {
        let message = Plan::parse("Filter TRUE::BOOLEAN\n").unwrap_err().to_string();
        assert!(message.contains("expected an operator"), "unhelpful message: {message}");
    }

    #[test]
    fn a_second_root_is_an_error() {
        let message = Plan::parse("Dummy\nDummy\n").unwrap_err().to_string();
        assert!(message.contains("past the end"), "unhelpful message: {message}");
    }

    #[test]
    fn text_left_over_on_a_line_is_an_error() {
        let message = Plan::parse("Dummy nonsense\n").unwrap_err().to_string();
        assert!(message.contains("left over"), "unhelpful message: {message}");
    }

    #[test]
    fn an_empty_plan_is_not_a_plan() {
        assert!(Plan::parse("").is_err());
        assert!(Plan::parse("\n\n  \n").is_err());
    }

    /// The reader has to reject what the validator rejects, or a hand-written dump becomes a way
    /// to build a plan nobody could build through the arena.
    #[test]
    fn the_reader_runs_the_plan_invariant() {
        let message = Plan::parse("Filter 1::INTEGER\n  Dummy\n").unwrap_err().to_string();
        assert!(message.contains("BOOLEAN"), "unhelpful message: {message}");
    }

    /// The printer quotes a function whose name is a reserved word. If that quoting did not buy a
    /// different reading here it would be quoting for nothing, and a function called `cast` would
    /// come back as a cast with the wrong number of operands.
    #[test]
    fn a_quoted_reserved_word_is_a_function_and_not_syntax() {
        for reserved in RESERVED {
            let text = format!("Project #1 [\"{reserved}\"(1::INTEGER)::INTEGER AS a]\n  Dummy\n");
            let plan = Plan::parse(&text).unwrap_or_else(|e| panic!("{text} does not read: {e}"));
            assert!(matches!(plan.expr(0), Expr::Constant(_)));
            assert!(matches!(plan.expr(1), Expr::Function { .. }), "{reserved} became syntax");
            assert_eq!(plan.to_string(), text);
        }
    }

    #[test]
    fn an_unquoted_cast_is_syntax_and_not_a_function() {
        let text = "Project #1 [CAST(1::INTEGER)::BIGINT AS a]\n  Dummy\n";
        let plan = Plan::parse(text).expect("a cast reads back");
        assert!(matches!(plan.expr(1), Expr::Cast { try_cast: false, .. }));
        assert_eq!(plan.to_string(), text);
    }

    /// `FILTER` and `DISTINCT` stand where an argument could stand, so a function of the same name
    /// in argument position is the case that tells whether the keyword check is precise enough.
    #[test]
    fn a_function_called_filter_in_argument_position_is_a_call() {
        let text = "Aggregate #1 groups=[] aggregates=[sum(filter(1::INTEGER)::INTEGER)::HUGEINT]\n  Dummy\n";
        let plan = Plan::parse(text).expect("an argument called filter reads back");
        assert_eq!(plan.to_string(), text);
    }

    #[test]
    fn an_aggregate_with_no_arguments_can_still_have_a_filter() {
        let text = "Aggregate #1 groups=[] aggregates=[count_star(FILTER TRUE::BOOLEAN)::BIGINT]\n  Dummy\n";
        let plan = Plan::parse(text).expect("count(*) FILTER (WHERE p) is a real query");
        assert_eq!(plan.to_string(), text);
    }

    #[test]
    fn a_name_that_is_not_a_plain_identifier_is_quoted_by_the_printer_and_read_here() {
        let mut c = Cursor::new("\"say \"\"hi\"\"\" rest");
        assert_eq!(read_identifier(&mut c).unwrap(), "say \"hi\"");
        assert_eq!(c.rest(), " rest");
        assert!(!is_plain_identifier("say \"hi\""));
    }
}
