//! The textual form.
//!
//! One line per operator, two spaces of indent per level, parent before children. Every expression
//! is written `form::TYPE`.
//!
//! The annotation is on every expression rather than only where a reader would need one. The
//! alternative is a reader that re-derives types, and re-deriving the type of `upper(x)` means
//! consulting the function catalog, and a dump that cannot be read back without a catalog is not a
//! dump. It costs width and it buys a reader that is a pure function of the text.
//!
//! Nothing in here allocates a plan-sized string. It writes into whatever
//! [`fmt::Write`](std::fmt::Write) it is handed, which for `to_string` is one growing buffer and
//! for a test comparison can be a sink that never keeps anything.

use std::fmt::{self, Write};

#[cfg(test)]
use rudb_common::LogicalType;
use rudb_common::Value;

use crate::expr::Expr;
use crate::node::Node;
use crate::plan::Plan;
use crate::{ExprRef, NodeRef, Slice};

/// Names that mean something in an expression, which a function of the same name has to be quoted
/// to get past. The reader looks for these unquoted and only unquoted, so `"cast"(x)` is a call to
/// a function called `cast` and `CAST(x)` is a cast.
pub(crate) const RESERVED: [&str; 3] = ["CAST", "TRY_CAST", "CASE"];

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_node(self, f, self.root(), 0)
    }
}

fn write_node<W: Write>(plan: &Plan, out: &mut W, node: NodeRef, depth: usize) -> fmt::Result {
    for _ in 0..depth {
        out.write_str("  ")?;
    }
    let held = plan.node(node);
    out.write_str(held.keyword())?;
    write_arguments(plan, out, held)?;
    out.write_char('\n')?;
    for child in held.children().into_iter().flatten() {
        write_node(plan, out, child, depth + 1)?;
    }
    Ok(())
}

fn write_arguments<W: Write>(plan: &Plan, out: &mut W, node: &Node) -> fmt::Result {
    match *node {
        Node::Dummy | Node::CrossProduct { .. } => Ok(()),
        Node::Get { catalog, schema, table, alias, index, columns } => {
            out.write_char(' ')?;
            write_identifier(out, plan.string(catalog))?;
            out.write_char('.')?;
            write_identifier(out, plan.string(schema))?;
            out.write_char('.')?;
            write_identifier(out, plan.string(table))?;
            out.write_str(" AS ")?;
            write_identifier(out, plan.string(alias))?;
            write!(out, " #{index} ")?;
            write_schema(plan, out, columns)
        }
        Node::Values { index, columns, rows } => {
            write!(out, " #{index} ")?;
            write_schema(plan, out, columns)?;
            out.write_str(" rows=[")?;
            for (position, row) in plan.row_list(rows).iter().enumerate() {
                if position > 0 {
                    out.write_str(", ")?;
                }
                write_expr_list(plan, out, *row)?;
            }
            out.write_char(']')
        }
        Node::TableFunction { index, function, args, columns } => {
            out.write_char(' ')?;
            write_identifier(out, plan.string(function))?;
            out.write_str(" args=")?;
            write_expr_list(plan, out, args)?;
            write!(out, " #{index} ")?;
            write_schema(plan, out, columns)
        }
        Node::Filter { predicate, .. } => {
            out.write_char(' ')?;
            write_expr(plan, out, predicate)
        }
        Node::Project { index, exprs, names, .. } => {
            write!(out, " #{index} [")?;
            for (position, &expr) in plan.expr_list(exprs).iter().enumerate() {
                if position > 0 {
                    out.write_str(", ")?;
                }
                write_expr(plan, out, expr)?;
                out.write_str(" AS ")?;
                write_identifier(out, plan.string(plan.name_list(names)[position]))?;
            }
            out.write_char(']')
        }
        Node::Aggregate { index, groups, aggregates, .. } => {
            write!(out, " #{index} groups=")?;
            write_expr_list(plan, out, groups)?;
            out.write_str(" aggregates=")?;
            write_expr_list(plan, out, aggregates)
        }
        Node::Sort { keys, .. } => write_sort_keys(plan, out, keys),
        Node::Limit { count, offset, .. } => {
            match count {
                Some(count) => write!(out, " {count}")?,
                None => out.write_str(" ALL")?,
            }
            write!(out, " offset {offset}")
        }
        Node::TopN { keys, count, offset, .. } => {
            write!(out, " {count} offset {offset}")?;
            write_sort_keys(plan, out, keys)
        }
        Node::Distinct { on, .. } => {
            out.write_str(" on=")?;
            write_expr_list(plan, out, on)
        }
        Node::Join { kind, conditions, .. } => {
            write!(out, " {} on=", kind.keyword())?;
            write_expr_list(plan, out, conditions)
        }
        Node::SetOp { kind, all, index, .. } => {
            let quantifier = if all { "ALL" } else { "DISTINCT" };
            write!(out, " {} {quantifier} #{index}", kind.keyword())
        }
    }
}

/// A named and typed column list, which is what a scan and a `VALUES` produce.
fn write_schema<W: Write>(plan: &Plan, out: &mut W, columns: Slice) -> fmt::Result {
    out.write_char('[')?;
    for (position, field) in plan.field_list(columns).iter().enumerate() {
        if position > 0 {
            out.write_str(", ")?;
        }
        write_identifier(out, &field.name)?;
        write!(out, "::{}", field.ty)?;
    }
    out.write_char(']')
}

/// The keys of a sort, in priority order, each with its direction and its null placement.
fn write_sort_keys<W: Write>(plan: &Plan, out: &mut W, keys: Slice) -> fmt::Result {
    out.write_str(" [")?;
    for (position, key) in plan.sort_key_list(keys).iter().enumerate() {
        if position > 0 {
            out.write_str(", ")?;
        }
        write_expr(plan, out, key.expr)?;
        out.write_str(if key.descending { " DESC" } else { " ASC" })?;
        out.write_str(if key.nulls_first { " NULLS FIRST" } else { " NULLS LAST" })?;
    }
    out.write_char(']')
}

fn write_expr_list<W: Write>(plan: &Plan, out: &mut W, list: Slice) -> fmt::Result {
    out.write_char('[')?;
    for (position, &expr) in plan.expr_list(list).iter().enumerate() {
        if position > 0 {
            out.write_str(", ")?;
        }
        write_expr(plan, out, expr)?;
    }
    out.write_char(']')
}

fn write_expr<W: Write>(plan: &Plan, out: &mut W, expr: ExprRef) -> fmt::Result {
    write_form(plan, out, expr)?;
    write!(out, "::{}", plan.expr_type(expr))
}

fn write_form<W: Write>(plan: &Plan, out: &mut W, expr: ExprRef) -> fmt::Result {
    match *plan.expr(expr) {
        Expr::Column(binding) => write!(out, "#{}.{}", binding.table, binding.column),
        Expr::Constant(value) => write_value(out, plan.value(value)),
        Expr::Cast { input, try_cast } => {
            out.write_str(if try_cast { "TRY_CAST(" } else { "CAST(" })?;
            write_expr(plan, out, input)?;
            out.write_char(')')
        }
        Expr::Compare { op, left, right } => {
            out.write_char('(')?;
            write_expr(plan, out, left)?;
            write!(out, " {} ", op.symbol())?;
            write_expr(plan, out, right)?;
            out.write_char(')')
        }
        Expr::Conjunction { op, children } => {
            out.write_char('(')?;
            for (position, &child) in plan.expr_list(children).iter().enumerate() {
                if position > 0 {
                    write!(out, " {} ", op.keyword())?;
                }
                write_expr(plan, out, child)?;
            }
            out.write_char(')')
        }
        Expr::Function { name, args } => {
            write_function_name(out, plan.string(name))?;
            out.write_char('(')?;
            write_arguments_of(plan, out, args)?;
            out.write_char(')')
        }
        Expr::Aggregate { name, args, distinct, filter } => {
            write_function_name(out, plan.string(name))?;
            out.write_char('(')?;
            if distinct {
                out.write_str("DISTINCT ")?;
            }
            write_arguments_of(plan, out, args)?;
            if let Some(filter) = filter {
                // No leading space when there are no arguments, because `count_star( FILTER x)`
                // has a space where an argument would go and reads as one that went missing.
                if !plan.expr_list(args).is_empty() {
                    out.write_char(' ')?;
                }
                out.write_str("FILTER ")?;
                write_expr(plan, out, filter)?;
            }
            out.write_char(')')
        }
        Expr::Case { arms, otherwise } => {
            out.write_str("CASE")?;
            for arm in plan.arm_list(arms) {
                out.write_str(" WHEN ")?;
                write_expr(plan, out, arm.when)?;
                out.write_str(" THEN ")?;
                write_expr(plan, out, arm.then)?;
            }
            if let Some(otherwise) = otherwise {
                out.write_str(" ELSE ")?;
                write_expr(plan, out, otherwise)?;
            }
            out.write_str(" END")
        }
    }
}

fn write_arguments_of<W: Write>(plan: &Plan, out: &mut W, args: Slice) -> fmt::Result {
    for (position, &arg) in plan.expr_list(args).iter().enumerate() {
        if position > 0 {
            out.write_str(", ")?;
        }
        write_expr(plan, out, arg)?;
    }
    Ok(())
}

/// Writes a constant.
///
/// The type annotation that follows is what says which of these a run of digits is, so nothing
/// here has to be self describing. `19723::DATE` is a day number rather than `'2024-01-15'`,
/// deliberately: a plan dump is diffed by a machine and compared by a test, the day number is what
/// the executor actually holds, and a date formatter in the round trip is a second place for a
/// calendar bug to live. [`Value`]'s own `Display` is DuckDB's user-facing rendering and is where
/// a person reading a result set gets a date from.
fn write_value<W: Write>(out: &mut W, value: &Value) -> fmt::Result {
    match value {
        Value::Null => out.write_str("NULL"),
        Value::Boolean(held) => out.write_str(if *held { "TRUE" } else { "FALSE" }),
        Value::TinyInt(held) => write!(out, "{held}"),
        Value::SmallInt(held) => write!(out, "{held}"),
        Value::Integer(held) => write!(out, "{held}"),
        Value::BigInt(held) => write!(out, "{held}"),
        Value::HugeInt(held) => write!(out, "{held}"),
        Value::UTinyInt(held) => write!(out, "{held}"),
        Value::USmallInt(held) => write!(out, "{held}"),
        Value::UInteger(held) => write!(out, "{held}"),
        Value::UBigInt(held) => write!(out, "{held}"),
        Value::UHugeInt(held) => write!(out, "{held}"),
        // The debug formatting of a float is the shortest text that reads back as the same bits,
        // which the display formatting is not: `{}` prints 0.1f32 as 0.1 and so does 0.1f64, and
        // those are different numbers.
        Value::Float(held) => write!(out, "{held:?}"),
        Value::Double(held) => write!(out, "{held:?}"),
        Value::Decimal { unscaled, scale, .. } => out.write_str(&decimal_text(*unscaled, *scale)),
        Value::Varchar(held) => write_string(out, held),
        Value::Blob(held) => {
            out.write_str("X'")?;
            for byte in held {
                write!(out, "{byte:02x}")?;
            }
            out.write_char('\'')
        }
        Value::Date(held) => write!(out, "{held}"),
        Value::Time(held) | Value::Timestamp(held) => write!(out, "{held}"),
        Value::Interval { months, days, micros } => write!(out, "{{{months}, {days}, {micros}}}"),
        Value::List { values, .. } => {
            out.write_char('{')?;
            for (position, element) in values.iter().enumerate() {
                if position > 0 {
                    out.write_str(", ")?;
                }
                write_value(out, element)?;
            }
            out.write_char('}')
        }
        // The field names are in the type annotation, which is where the reader takes them from,
        // so writing them again here would be a second copy that can disagree with the first.
        Value::Struct(fields) => {
            out.write_char('{')?;
            for (position, (_, held)) in fields.iter().enumerate() {
                if position > 0 {
                    out.write_str(", ")?;
                }
                write_value(out, held)?;
            }
            out.write_char('}')
        }
        // Value is non_exhaustive, so a variant added in rudb-common lands here with no form of
        // its own. Writing something the reader is guaranteed to reject is the loudest option
        // available: the round trip test fails on the value that has no form rather than the dump
        // quietly becoming a thing that cannot be read back.
        other => write!(out, "<no textual form for {other:?}>"),
    }
}

/// The digits of a decimal with the point where the scale says it is.
///
/// The unscaled integer is what the value holds and printing that instead would round trip just as
/// exactly, but `1234::DECIMAL(6,2)` is a number nobody can read and `12.34::DECIMAL(6,2)` is the
/// same information.
pub(crate) fn decimal_text(unscaled: i128, scale: u8) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }
    let scale = usize::from(scale);
    let digits = unscaled.unsigned_abs().to_string();
    // A value smaller than one unit needs leading zeros before the point, so 5 at scale 3 is 0.005
    // and not .005 or 5.000.
    let padded = if digits.len() <= scale {
        format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
    } else {
        digits
    };
    let point = padded.len() - scale;
    let sign = if unscaled < 0 { "-" } else { "" };
    format!("{sign}{}.{}", &padded[..point], &padded[point..])
}

/// Writes a string constant, single quoted, with the quote doubled.
///
/// A control character goes out as `\xNN` and a backslash doubles, because a dump is compared line
/// by line and a value holding a newline would otherwise turn one operator into two lines and the
/// reader would see an indent that does not exist.
fn write_string<W: Write>(out: &mut W, text: &str) -> fmt::Result {
    out.write_char('\'')?;
    for character in text.chars() {
        match character {
            '\'' => out.write_str("''")?,
            '\\' => out.write_str("\\\\")?,
            control if control.is_control() => write!(out, "\\x{:02x}", control as u32)?,
            other => out.write_char(other)?,
        }
    }
    out.write_char('\'')
}

/// Whether a name reads back unquoted.
pub(crate) fn is_plain_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Writes a name, quoting it if it would not survive being read back unquoted.
fn write_identifier<W: Write>(out: &mut W, name: &str) -> fmt::Result {
    if is_plain_identifier(name) {
        return out.write_str(name);
    }
    out.write_char('"')?;
    for character in name.chars() {
        if character == '"' {
            out.write_str("\"\"")?;
        } else {
            out.write_char(character)?;
        }
    }
    out.write_char('"')
}

/// Writes a function name, which additionally has to get past the reserved words.
fn write_function_name<W: Write>(out: &mut W, name: &str) -> fmt::Result {
    if RESERVED.iter().any(|reserved| name.eq_ignore_ascii_case(reserved)) {
        return write!(out, "\"{name}\"");
    }
    write_identifier(out, name)
}

/// Whether a type prints as something the reader can find the end of.
///
/// The reader takes a type annotation as a name, then a balanced parenthesis group, then any
/// number of balanced bracket groups, then optionally `WITH TIME ZONE`. Every type
/// [`LogicalType`]'s own `Display` produces fits that, and this is the assertion that says so, for
/// the test that walks the whole type set.
#[cfg(test)]
pub(crate) fn prints_readably(ty: &LogicalType) -> bool {
    let text = ty.to_string();
    crate::parse::type_extent(&text, 0) == text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decimal_gets_its_point_from_its_scale() {
        assert_eq!(decimal_text(1234, 2), "12.34");
        assert_eq!(decimal_text(1234, 0), "1234");
        assert_eq!(decimal_text(5, 3), "0.005");
        assert_eq!(decimal_text(-5, 3), "-0.005");
        assert_eq!(decimal_text(-1234, 2), "-12.34");
        assert_eq!(decimal_text(0, 2), "0.00");
    }

    #[test]
    fn the_widest_decimal_still_prints() {
        let widest = 10i128.pow(37) - 1;
        assert_eq!(decimal_text(widest, 0).len(), 37);
        assert_eq!(decimal_text(widest, 37).len(), 39);
    }

    #[test]
    fn a_name_that_needs_quoting_gets_it() {
        let quoted = |name: &str| {
            let mut out = String::new();
            write_identifier(&mut out, name).unwrap();
            out
        };
        assert_eq!(quoted("SearchPhrase"), "SearchPhrase");
        assert_eq!(quoted("_hidden9"), "_hidden9");
        assert_eq!(quoted("a b"), "\"a b\"");
        assert_eq!(quoted("9lives"), "\"9lives\"", "a name cannot start with a digit");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn a_function_named_after_a_reserved_word_is_quoted() {
        for name in ["cast", "CAST", "Try_Cast", "case"] {
            let mut out = String::new();
            write_function_name(&mut out, name).unwrap();
            assert!(out.starts_with('"'), "{name} would be read back as syntax");
        }
        let mut out = String::new();
        write_function_name(&mut out, "casting").unwrap();
        assert_eq!(out, "casting", "only the reserved words themselves are reserved");
    }

    #[test]
    fn a_string_never_contains_a_newline_when_it_is_written() {
        let mut out = String::new();
        write_string(&mut out, "one\ntwo\ttab'quote\\slash").unwrap();
        assert!(!out.contains('\n'), "a value would split an operator across two lines");
        assert_eq!(out, "'one\\x0atwo\\x09tab''quote\\\\slash'");
    }

    /// Floats are the one value kind where the obvious formatting is wrong, and it is wrong
    /// quietly: `{}` on the nearest f32 to 0.1 prints 0.1, and 0.1 read back as f32 is a different
    /// number than the one that was printed.
    #[test]
    fn a_float_prints_the_text_that_reads_back_as_the_same_bits() {
        for held in [0.1f32, f32::MIN, f32::MAX, f32::EPSILON, -0.0, 1e-40] {
            let mut out = String::new();
            write_value(&mut out, &Value::Float(held)).unwrap();
            let back: f32 = out.parse().expect("a float we printed parses");
            assert_eq!(back.to_bits(), held.to_bits(), "{out} is not the same float");
        }
        for held in [0.1f64, f64::MIN, f64::MAX, f64::EPSILON, -0.0, 1e-308] {
            let mut out = String::new();
            write_value(&mut out, &Value::Double(held)).unwrap();
            let back: f64 = out.parse().expect("a double we printed parses");
            assert_eq!(back.to_bits(), held.to_bits(), "{out} is not the same double");
        }
    }

    #[test]
    fn a_plan_that_is_only_a_dummy_prints_one_line() {
        assert_eq!(Plan::new().to_string(), "Dummy\n");
    }

    /// The reader finds the end of a type annotation by scanning rather than by parsing, and the
    /// scan knows four shapes: a name, a balanced parenthesis group, balanced bracket groups, and
    /// the `WITH TIME ZONE` suffix. A type that prints as something outside those four is a type
    /// that swallows whatever comes after it in the dump, which shows up as a syntax error on the
    /// far side of the line rather than as anything to do with the type.
    #[test]
    fn every_type_prints_as_something_the_reader_can_find_the_end_of() {
        let scalars = [
            LogicalType::Null,
            LogicalType::Boolean,
            LogicalType::TinyInt,
            LogicalType::SmallInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UTinyInt,
            LogicalType::USmallInt,
            LogicalType::UInteger,
            LogicalType::UBigInt,
            LogicalType::UHugeInt,
            LogicalType::Float,
            LogicalType::Double,
            LogicalType::Varchar,
            LogicalType::Blob,
            LogicalType::Bit,
            LogicalType::Uuid,
            LogicalType::Date,
            LogicalType::Time,
            LogicalType::TimeTz,
            LogicalType::Timestamp,
            LogicalType::TimestampS,
            LogicalType::TimestampMs,
            LogicalType::TimestampNs,
            LogicalType::TimestampTz,
            LogicalType::Interval,
        ];
        let mut all: Vec<LogicalType> = scalars.to_vec();
        all.push(LogicalType::decimal(18, 3).expect("18 and 3 is a decimal"));
        all.push(LogicalType::decimal(38, 0).expect("the widest decimal"));
        for scalar in &scalars {
            all.push(LogicalType::list(scalar.clone()));
            all.push(LogicalType::array(scalar.clone(), 4));
            all.push(LogicalType::map(LogicalType::Varchar, scalar.clone()));
            all.push(LogicalType::Struct(vec![
                rudb_common::Field::new("a", scalar.clone()),
                rudb_common::Field::new("b b", LogicalType::Varchar),
            ]));
            all.push(LogicalType::Union(vec![rudb_common::Field::new("u", scalar.clone())]));
        }
        all.push(LogicalType::list(LogicalType::list(LogicalType::Integer)));
        all.push(LogicalType::list(LogicalType::map(
            LogicalType::Varchar,
            LogicalType::TimestampTz,
        )));

        for ty in all {
            let text = ty.to_string();
            assert!(prints_readably(&ty), "the reader cannot find the end of {text}");
            let back = LogicalType::parse(&text)
                .unwrap_or_else(|error| panic!("{text} does not parse: {error}"));
            assert_eq!(back, ty, "{text} does not read back as itself");
        }
    }
}
