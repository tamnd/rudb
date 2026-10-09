//! Expressions.
//!
//! The grammar of PostgreSQL writes some expressions as calls to functions of `pg_catalog`, for
//! example `EXTRACT`, `TRIM` and `AT TIME ZONE`, and marks them with `COERCE_SQL_SYNTAX`. They are
//! given back here as the expression that the DuckDB transform makes for the same text, so the
//! binder has one form of each.

use rudb_common::{Error, SqlState};
use rudb_parse::NONE;
use rudb_parse::ast::{
    BinaryOp, CaseArm, Expr, ExprRef, LiteralKind, OrderItem, Slice, Target, UnaryOp,
};

use super::{Made, Place, Transform, clause, not_yet};
use crate::nodes::{
    A_Const, A_Expr, A_Expr_Kind, A_Indirection, BoolExpr, BoolExprType, BoolTestType,
    CoercionForm, FuncCall, List, MinMaxOp, Node, NullTestType, SQLValueFunction,
    SQLValueFunctionOp, SubLink, SubLinkType,
};
use crate::token;

impl Transform<'_> {
    pub(super) fn expr(&mut self, node: &Node) -> Made<ExprRef> {
        match node {
            Node::A_Const(constant) => self.constant(constant),
            Node::ColumnRef(column) => self.column(&column.fields, column.location),
            Node::ParamRef(param) => {
                let name = self.intern(&param.number.to_string());
                Ok(self.push(Expr::Parameter { name }, param.location))
            }
            Node::A_Expr(a) => self.a_expr(a),
            Node::BoolExpr(b) => self.bool_expr(b),
            Node::SubLink(link) => self.sublink(link),
            Node::CaseExpr(case) => {
                let operand = self.optional(case.arg.as_ref())?;
                let mut arms = Vec::with_capacity(case.args.len());
                for node in case.args.iter().flatten() {
                    let Node::CaseWhen(arm) = node else {
                        return Err(not_yet(node));
                    };
                    let (Some(when), Some(then)) = (&arm.expr, &arm.result) else {
                        return clause("CaseWhen");
                    };
                    arms.push(CaseArm { when: self.expr(when)?, then: self.expr(then)? });
                }
                let arms = self.ast.case_slice(arms);
                let otherwise = self.optional(case.defresult.as_ref())?;
                Ok(self.push(Expr::Case { operand, arms, otherwise }, case.location))
            }
            Node::NullTest(test) => {
                let op = if test.nulltesttype == NullTestType::IS_NULL {
                    UnaryOp::IsNull
                } else {
                    UnaryOp::IsNotNull
                };
                self.unary(op, test.arg.as_ref(), test.location)
            }
            Node::BooleanTest(test) => {
                let op = match test.booltesttype {
                    BoolTestType::IS_TRUE => UnaryOp::IsTrue,
                    BoolTestType::IS_NOT_TRUE => UnaryOp::IsNotTrue,
                    BoolTestType::IS_FALSE => UnaryOp::IsFalse,
                    BoolTestType::IS_NOT_FALSE => UnaryOp::IsNotFalse,
                    BoolTestType::IS_UNKNOWN => UnaryOp::IsUnknown,
                    _ => UnaryOp::IsNotUnknown,
                };
                self.unary(op, test.arg.as_ref(), test.location)
            }
            Node::CoalesceExpr(call) => self.call("coalesce", &call.args, call.location),
            Node::MinMaxExpr(call) => {
                let name = if call.op == MinMaxOp::IS_GREATEST { "greatest" } else { "least" };
                self.call(name, &call.args, call.location)
            }
            Node::GroupingFunc(call) => self.call("grouping", &call.args, call.location),
            Node::A_ArrayExpr(array) => {
                let items = self.expr_list(&array.elements)?;
                let list = self.push(Expr::List { items }, array.location);
                // An inner list of `ARRAY[[1], [2]]` starts at its bracket, and a list written
                // with the word starts at the word.
                let at = usize::try_from(array.location).ok();
                if at.and_then(|at| self.text.as_bytes().get(at)) != Some(&b'[') {
                    self.ast.array_lists.push(list);
                }
                Ok(list)
            }
            Node::RowExpr(row) => {
                let items = self.row_items(&row.args, true)?;
                Ok(self.push(Expr::Row { items }, row.location))
            }
            Node::TypeCast(cast) => {
                let (Some(arg), Some(name)) = (&cast.arg, &cast.typeName) else {
                    return clause("TypeCast");
                };
                let operand = self.expr(arg)?;
                let ty = self.type_name(name)?;
                let ty = self.intern(&ty);
                Ok(self.push(Expr::Cast { operand, ty, try_cast: false }, cast.location))
            }
            Node::A_Indirection(indirection) => self.indirection(indirection),
            Node::CollateClause(collate) => {
                let Some(arg) = &collate.arg else {
                    return clause("CollateClause");
                };
                let left = self.expr(arg)?;
                let name = self.names(&collate.collname)?;
                let right = self.push(Expr::Column { name }, collate.location);
                let op = BinaryOp::Collate;
                Ok(self.push(Expr::Binary { op, left, right }, collate.location))
            }
            Node::SQLValueFunction(value) => self.value_function(value),
            Node::FuncCall(call) => self.function(call),
            Node::SetToDefault(default) => Ok(self.push(Expr::Default, default.location)),
            node => Err(not_yet(node)),
        }
    }

    /// An expression, or `NONE` when there is none.
    pub(super) fn optional(&mut self, node: Option<&Node>) -> Made<ExprRef> {
        match node {
            Some(node) => self.expr(node),
            None => Ok(NONE),
        }
    }

    pub(super) fn expr_list(&mut self, list: &[Option<Node>]) -> Made<Slice> {
        let mut items = Vec::with_capacity(list.len());
        for node in list.iter().flatten() {
            items.push(self.expr(node)?);
        }
        Ok(self.ast.expr_slice(items))
    }

    /// The items of a row constructor or the arguments of a call, where `t.*` is not a star.
    ///
    /// PostgreSQL reads `t.*` there as the whole row of `t`. A row constructor takes the row apart
    /// into its fields, as `(t).*` does, so `row(t.*, 3)` has a field for each column of `t` and
    /// then the `3`. A call takes it as one value, so `row_to_json(t.*)` is `row_to_json(t)`.
    fn row_items(&mut self, list: &[Option<Node>], expand: bool) -> Made<Slice> {
        let mut items = Vec::with_capacity(list.len());
        for node in list.iter().flatten() {
            let Node::ColumnRef(column) = node else {
                items.push(self.expr(node)?);
                continue;
            };
            let fields = &column.fields;
            let (Some(Some(Node::A_Star(_))), 2..) = (fields.last(), fields.len()) else {
                items.push(self.expr(node)?);
                continue;
            };
            let name = self.names(&fields[..fields.len() - 1])?;
            let mut item = self.push(Expr::Column { name }, column.location);
            if expand {
                item = self.push(Expr::Fields { record: item }, column.location);
            }
            items.push(item);
        }
        Ok(self.ast.expr_slice(items))
    }

    fn unary(&mut self, op: UnaryOp, operand: Option<&Node>, location: i32) -> Made<ExprRef> {
        let Some(operand) = operand else {
            return clause("Unary");
        };
        let operand = self.expr(operand)?;
        Ok(self.push(Expr::Unary { op, operand }, location))
    }

    fn binary(&mut self, op: BinaryOp, left: ExprRef, right: ExprRef, location: i32) -> ExprRef {
        self.push(Expr::Binary { op, left, right }, location)
    }

    /// A call that the transform makes, with a name of one part.
    fn call(&mut self, name: &str, args: &List, location: i32) -> Made<ExprRef> {
        let args = self.expr_list(args)?;
        Ok(self.made_call(name, args, location))
    }

    fn made_call(&mut self, name: &str, args: Slice, location: i32) -> ExprRef {
        let part = self.intern(name);
        let name = self.ast.part_slice([part]);
        self.push(Expr::Function { name, args, distinct: false, filter: NONE }, location)
    }

    fn number(&mut self, text: &str, location: i32) -> ExprRef {
        let text = self.intern(text);
        self.push(Expr::Literal { kind: LiteralKind::Number, text }, location)
    }

    /// The omitted bound of a slice.
    fn omitted(&mut self) -> ExprRef {
        let items = self.ast.expr_slice(Vec::new());
        self.push(Expr::List { items }, -1)
    }

    fn string(&mut self, text: &str, location: i32) -> ExprRef {
        let text = self.intern(text);
        self.push(Expr::Literal { kind: LiteralKind::String, text }, location)
    }

    /// A constant. The grammar folds a minus sign into a number, and it is given back as a
    /// negation here, which is what the DuckDB transform makes of `-1`.
    fn constant(&mut self, constant: &A_Const) -> Made<ExprRef> {
        let location = constant.location;
        if constant.isnull {
            let literal = Expr::Literal { kind: LiteralKind::Null, text: NONE };
            return Ok(self.push(literal, location));
        }
        let (digits, negative) = match &constant.val {
            Some(Node::Integer(value)) => (i64::from(*value).abs().to_string(), *value < 0),
            Some(Node::Float(text)) => match text.strip_prefix('-') {
                Some(digits) => (digits.to_string(), true),
                None => (text.to_string(), false),
            },
            Some(Node::String(text)) => return Ok(self.string(text, location)),
            Some(Node::Boolean(value)) => {
                let kind = if *value { LiteralKind::True } else { LiteralKind::False };
                return Ok(self.push(Expr::Literal { kind, text: NONE }, location));
            }
            Some(node) => return Err(not_yet(node)),
            None => return clause("A_Const"),
        };
        let number = self.number(&digits, location);
        if !negative {
            return Ok(number);
        }
        Ok(self.push(Expr::Unary { op: UnaryOp::Negate, operand: number }, location))
    }

    /// A column, or a star with the names before it.
    fn column(&mut self, fields: &List, location: i32) -> Made<ExprRef> {
        if let Some(Some(Node::A_Star(_))) = fields.last() {
            let qualifier = self.names(&fields[..fields.len() - 1])?;
            let star = Expr::Star { qualifier, replacements: Slice::default() };
            return Ok(self.push(star, location));
        }
        let name = self.names(fields)?;
        Ok(self.push(Expr::Column { name }, location))
    }

    /// The symbol of an operator name. A name with a schema is valid only when the schema is
    /// `pg_catalog`, which is where the operators of the dialect are.
    pub(super) fn symbol(name: &List) -> Made<&str> {
        match Self::strings(name)?[..] {
            [symbol] | ["pg_catalog", symbol] => Ok(symbol),
            _ => clause("QualifiedOperator"),
        }
    }

    fn operator(&mut self, symbol: &str) -> BinaryOp {
        rudb_parse::build::symbol_op(symbol).unwrap_or_else(|| BinaryOp::Named(self.intern(symbol)))
    }

    fn a_expr(&mut self, a: &A_Expr) -> Made<ExprRef> {
        let location = a.location;
        let symbol = Self::symbol(&a.name)?;
        let (left, right) = (a.lexpr.as_ref(), a.rexpr.as_ref());
        match a.kind {
            A_Expr_Kind::AEXPR_OP => {
                let Some(left) = left else {
                    let op = match symbol {
                        "-" => UnaryOp::Negate,
                        "+" => UnaryOp::Plus,
                        "~" => UnaryOp::BitNot,
                        _ => UnaryOp::Named(self.intern(symbol)),
                    };
                    return self.unary(op, right, location);
                };
                let Some(right) = right else {
                    return clause("PostfixOperator");
                };
                let op = self.operator(symbol);
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                Ok(self.binary(op, left, right, location))
            }
            A_Expr_Kind::AEXPR_OP_ANY | A_Expr_Kind::AEXPR_OP_ALL => {
                let (Some(left), Some(right)) = (left, right) else {
                    return clause("A_Expr");
                };
                let op = self.operator(symbol);
                let operand = self.expr(left)?;
                let array = self.expr(right)?;
                let all = a.kind == A_Expr_Kind::AEXPR_OP_ALL;
                Ok(self.push(Expr::QuantifiedArray { operand, op, array, all }, location))
            }
            A_Expr_Kind::AEXPR_DISTINCT | A_Expr_Kind::AEXPR_NOT_DISTINCT => {
                let (Some(left), Some(right)) = (left, right) else {
                    return clause("A_Expr");
                };
                let op = if a.kind == A_Expr_Kind::AEXPR_DISTINCT {
                    BinaryOp::IsDistinctFrom
                } else {
                    BinaryOp::IsNotDistinctFrom
                };
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                Ok(self.binary(op, left, right, location))
            }
            A_Expr_Kind::AEXPR_NULLIF => {
                let (Some(left), Some(right)) = (left, right) else {
                    return clause("A_Expr");
                };
                let args = [self.expr(left)?, self.expr(right)?];
                let args = self.ast.expr_slice(args);
                Ok(self.made_call("nullif", args, location))
            }
            A_Expr_Kind::AEXPR_IN => {
                let (Some(left), Some(Node::List(list))) = (left, right) else {
                    return clause("A_Expr");
                };
                let operand = self.expr(left)?;
                let list = self.expr_list(list)?;
                let negated = symbol == "<>";
                Ok(self.push(Expr::In { operand, list, negated }, location))
            }
            A_Expr_Kind::AEXPR_LIKE | A_Expr_Kind::AEXPR_ILIKE => {
                let (Some(left), Some(right)) = (left, right) else {
                    return clause("A_Expr");
                };
                let negated = symbol.starts_with('!');
                if let Some([pattern, escape]) = escape_call(right, "like_escape") {
                    // A `LIKE` with an `ESCAPE` is a call with the escape as the third argument,
                    // and a `NOT` in front stays a negation, as in the DuckDB transform.
                    let name = if a.kind == A_Expr_Kind::AEXPR_LIKE {
                        "like_escape"
                    } else {
                        "ilike_escape"
                    };
                    let args = [self.expr(left)?, self.expr(pattern)?, self.expr(escape)?];
                    let args = self.ast.expr_slice(args);
                    let call = self.made_call(name, args, location);
                    if !negated {
                        return Ok(call);
                    }
                    let not = Expr::Unary { op: UnaryOp::Not, operand: call };
                    return Ok(self.push(not, location));
                }
                let op = self.operator(symbol);
                let left = self.expr(left)?;
                let right = self.expr(right)?;
                Ok(self.binary(op, left, right, location))
            }
            A_Expr_Kind::AEXPR_SIMILAR => {
                let (Some(left), Some(right)) = (left, right) else {
                    return clause("A_Expr");
                };
                // As in PostgreSQL, the pattern is a regular expression that `similar_to_escape`
                // makes, and the operator is `~` or `!~`.
                let Node::FuncCall(call) = right else {
                    return clause("SimilarEscape");
                };
                if escape_call::<1>(right, "similar_to_escape").is_none()
                    && escape_call::<2>(right, "similar_to_escape").is_none()
                {
                    return clause("SimilarEscape");
                }
                let op = self.operator(symbol);
                let left = self.expr(left)?;
                let right = self.call("similar_to_escape", &call.args, location)?;
                Ok(self.binary(op, left, right, location))
            }
            A_Expr_Kind::AEXPR_BETWEEN | A_Expr_Kind::AEXPR_NOT_BETWEEN => {
                let (Some(left), Some(Node::List(bounds))) = (left, right) else {
                    return clause("A_Expr");
                };
                let [Some(low), Some(high)] = &bounds[..] else {
                    return clause("A_Expr");
                };
                let operand = self.expr(left)?;
                let low = self.expr(low)?;
                let high = self.expr(high)?;
                let negated = a.kind == A_Expr_Kind::AEXPR_NOT_BETWEEN;
                Ok(self.push(Expr::Between { operand, low, high, negated }, location))
            }
            _ => clause("BetweenSymmetric"),
        }
    }

    fn bool_expr(&mut self, b: &BoolExpr) -> Made<ExprRef> {
        let location = b.location;
        let op = match b.boolop {
            BoolExprType::AND_EXPR => BinaryOp::And,
            BoolExprType::OR_EXPR => BinaryOp::Or,
            _ => {
                let [Some(arg)] = &b.args[..] else {
                    return clause("BoolExpr");
                };
                return self.not(arg, location);
            }
        };
        let mut args = b.args.iter().flatten();
        let Some(first) = args.next() else {
            return clause("BoolExpr");
        };
        let mut left = self.expr(first)?;
        for arg in args {
            let right = self.expr(arg)?;
            left = self.binary(op, left, right, location);
        }
        Ok(left)
    }

    /// `NOT`. The grammar writes `a NOT IN (SELECT ...)` as a `NOT` over the `IN`, at the same
    /// location. It is one node in the DuckDB transform, so it is one node here too. A `NOT` written
    /// before an expression, `NOT EXISTS` included, stays a `NOT`, as it does there.
    fn not(&mut self, arg: &Node, location: i32) -> Made<ExprRef> {
        if let Node::SubLink(link) = arg
            && link.subLinkType == SubLinkType::ANY_SUBLINK
            && link.operName.is_empty()
            && link.location == location
        {
            return self.sublink_negated(link, true);
        }
        self.unary(UnaryOp::Not, Some(arg), location)
    }

    fn sublink(&mut self, link: &SubLink) -> Made<ExprRef> {
        self.sublink_negated(link, false)
    }

    fn sublink_negated(&mut self, link: &SubLink, negated: bool) -> Made<ExprRef> {
        let location = link.location;
        let Some(Node::SelectStmt(select)) = &link.subselect else {
            return clause("SubLink");
        };
        let operand = match link.subLinkType {
            SubLinkType::ANY_SUBLINK | SubLinkType::ALL_SUBLINK => {
                let Some(test) = &link.testexpr else {
                    return clause("SubLink");
                };
                self.expr(test)?
            }
            SubLinkType::EXISTS_SUBLINK
            | SubLinkType::EXPR_SUBLINK
            | SubLinkType::ARRAY_SUBLINK => NONE,
            _ => return clause("SubLink"),
        };
        let query = self.within(Some(Place::Sublink), |this| this.query(select))?;
        let expr = match link.subLinkType {
            SubLinkType::EXISTS_SUBLINK => Expr::Exists { query, negated },
            SubLinkType::ANY_SUBLINK if link.operName.is_empty() => {
                Expr::InSubquery { operand, query, negated }
            }
            SubLinkType::ANY_SUBLINK | SubLinkType::ALL_SUBLINK => {
                let symbol = Self::symbol(&link.operName)?;
                let op = self.operator(symbol);
                let all = link.subLinkType == SubLinkType::ALL_SUBLINK;
                Expr::QuantifiedSubquery { operand, op, query, all }
            }
            SubLinkType::EXPR_SUBLINK => Expr::Subquery { query, array: false },
            _ => Expr::Subquery { query, array: true },
        };
        Ok(self.push(expr, location))
    }

    /// Subscripts and field selection: `a[1]`, `a[1:2]` and `(a).b`, as the calls the DuckDB
    /// transform makes of them. A star at the end, `(a).*`, is every field of what comes before
    /// it, which the grammar allows only last.
    fn indirection(&mut self, indirection: &A_Indirection) -> Made<ExprRef> {
        let Some(arg) = &indirection.arg else {
            return clause("A_Indirection");
        };
        let mut target = self.expr(arg)?;
        let span = self.ast.expr_span(target);
        for node in indirection.indirection.iter().flatten() {
            let mut selected = NONE;
            let (name, args) = match node {
                Node::A_Star(_) => {
                    target = self.ast.push_expr(Expr::Fields { record: target }, span);
                    continue;
                }
                Node::String(field) => {
                    selected = self.intern(field);
                    let field = self.string(field, -1);
                    ("struct_extract", vec![target, field])
                }
                Node::A_Indices(indices) if !indices.is_slice => {
                    let Some(index) = &indices.uidx else {
                        return clause("A_Indices");
                    };
                    ("array_extract", vec![target, self.expr(index)?])
                }
                // An omitted bound is an empty list, as the omitted step of DuckDB is. A written
                // bound is a number, and `a[1:-1]` of PostgreSQL is an empty slice and not the
                // whole array, so the binder must see which bound is omitted.
                Node::A_Indices(indices) => {
                    let first = match &indices.lidx {
                        Some(node) => self.expr(node)?,
                        None => self.omitted(),
                    };
                    let last = match &indices.uidx {
                        Some(node) => self.expr(node)?,
                        None => self.omitted(),
                    };
                    ("array_slice", vec![target, first, last])
                }
                node => return Err(not_yet(node)),
            };
            let args = self.ast.expr_slice(args);
            let part = self.intern(name);
            let name = self.ast.part_slice([part]);
            let call = Expr::Function { name, args, distinct: false, filter: NONE };
            target = self.ast.push_expr(call, span);
            self.ast.indirections.push((target, selected));
        }
        Ok(target)
    }

    /// `CURRENT_DATE` and the other words that PostgreSQL reads as a value. Each is a column of
    /// its name, which the binder resolves as the DuckDB transform has it, and a precision makes
    /// it a call.
    fn value_function(&mut self, value: &SQLValueFunction) -> Made<ExprRef> {
        let location = value.location;
        let name = match value.op {
            SQLValueFunctionOp::SVFOP_CURRENT_DATE => "current_date",
            SQLValueFunctionOp::SVFOP_CURRENT_TIME | SQLValueFunctionOp::SVFOP_CURRENT_TIME_N => {
                "current_time"
            }
            SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP
            | SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP_N => "current_timestamp",
            SQLValueFunctionOp::SVFOP_LOCALTIME | SQLValueFunctionOp::SVFOP_LOCALTIME_N => {
                "localtime"
            }
            SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP
            | SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP_N => "localtimestamp",
            SQLValueFunctionOp::SVFOP_CURRENT_ROLE => "current_role",
            SQLValueFunctionOp::SVFOP_CURRENT_USER => "current_user",
            SQLValueFunctionOp::SVFOP_USER => "user",
            SQLValueFunctionOp::SVFOP_SESSION_USER => "session_user",
            SQLValueFunctionOp::SVFOP_CURRENT_CATALOG => "current_catalog",
            _ => "current_schema",
        };
        let precise = matches!(
            value.op,
            SQLValueFunctionOp::SVFOP_CURRENT_TIME_N
                | SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP_N
                | SQLValueFunctionOp::SVFOP_LOCALTIME_N
                | SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP_N
        );
        if precise {
            let precision = self.number(&value.typmod.to_string(), -1);
            let args = self.ast.expr_slice([precision]);
            return Ok(self.made_call(name, args, location));
        }
        let part = self.intern(name);
        let name = self.ast.part_slice([part]);
        Ok(self.push(Expr::Column { name }, location))
    }

    pub(super) fn function(&mut self, call: &FuncCall) -> Made<ExprRef> {
        let location = call.location;
        if call.funcformat == CoercionForm::COERCE_SQL_SYNTAX {
            return self.syntax_call(call);
        }
        if call.agg_within_group {
            return self.within_group(call);
        }
        let named = call.args.iter().flatten().any(|node| matches!(node, Node::NamedArgExpr(_)));
        if named || call.func_variadic {
            let plain = call.over.is_none()
                && call.agg_order.is_empty()
                && call.agg_filter.is_none()
                && !call.agg_distinct
                && !call.agg_star;
            if !plain {
                return clause("Variadic");
            }
            return self.written_call(call);
        }
        let parts = Self::strings(&call.funcname)?;
        // These calls are rewritten by the DuckDB transform, and the rules move to
        // `rudb_parse::build` before this transform makes them.
        let rewritten = match parts[..] {
            ["struct_pack"] => true,
            ["struct_insert" | "struct_update" | "unnest"] => {
                !call.agg_order.is_empty() && call.over.is_none()
            }
            [.., "ifnull"] => call.over.is_none(),
            _ => false,
        };
        if rewritten {
            return clause("FuncCall");
        }
        let name = self.names(&call.funcname)?;
        let args = if call.agg_star {
            let star = Expr::Star { qualifier: Slice::default(), replacements: Slice::default() };
            let star = self.push(star, location);
            self.ast.expr_slice([star])
        } else {
            self.row_items(&call.args, false)?
        };
        let mut items: Vec<OrderItem> = Vec::with_capacity(call.agg_order.len());
        for node in call.agg_order.iter().flatten() {
            items.push(self.sort_by(node)?);
        }
        let order = self.ast.order_slice(items);
        let filter = self.optional(call.agg_filter.as_ref())?;
        let distinct = call.agg_distinct;
        // `IGNORE NULLS` is 1 and `RESPECT NULLS` is 2, which is the default. The binder refuses
        // either one on a call that cannot have it, so the call is kept to one side when it has
        // one.
        let treated = call.ignore_nulls != 0;
        let Some(over) = &call.over else {
            let made = self.push(Expr::Function { name, args, distinct, filter }, location);
            if order.len > 0 {
                self.ast.aggregate_orders.push((made, order));
            }
            if treated {
                self.ast.null_treated.push(made);
            }
            return Ok(made);
        };
        let spec = self.over(over)?;
        let ignore_nulls = call.ignore_nulls == 1;
        let window = Expr::Window { name, args, distinct, filter, ignore_nulls, order, spec };
        let made = self.push(window, location);
        if treated {
            self.ast.null_treated.push(made);
        }
        Ok(made)
    }

    /// An ordered-set call, `percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`. The grammar keeps the
    /// order of `WITHIN GROUP` where an `ORDER BY` inside the call goes, and the call keeps its
    /// written name and its direct arguments, with the order beside it, for the binder to resolve
    /// the way `ParseFuncOrColumn` does. A call with `OVER` or a null treatment is left to the
    /// other transform.
    fn within_group(&mut self, call: &FuncCall) -> Made<ExprRef> {
        if call.ignore_nulls != 0 {
            return clause("WithinGroup");
        }
        let name = self.names(&call.funcname)?;
        let args = self.row_items(&call.args, false)?;
        let mut items: Vec<OrderItem> = Vec::with_capacity(call.agg_order.len());
        for node in call.agg_order.iter().flatten() {
            items.push(self.sort_by(node)?);
        }
        let order = self.ast.order_slice(items);
        let filter = self.optional(call.agg_filter.as_ref())?;
        let function = Expr::Function { name, args, distinct: false, filter };
        let made = self.push(function, call.location);
        self.ast.within_groups.push((made, order));
        if call.over.is_some() {
            self.ast.windowed_within_groups.push(made);
        }
        Ok(made)
    }

    /// A plain call with named arguments or with `VARIADIC`, which the binder resolves by the rules
    /// of `func_get_detail`. The arguments before the first name are the arguments of the call, and
    /// the named ones are kept beside it in the order they were written. A positional argument
    /// after a name and a name given twice are the errors of `ParseFuncOrColumn`.
    fn written_call(&mut self, call: &FuncCall) -> Made<ExprRef> {
        let name = self.names(&call.funcname)?;
        let first = call.args.iter().position(|node| matches!(node, Some(Node::NamedArgExpr(_))));
        let first = first.unwrap_or(call.args.len());
        let args = self.row_items(&call.args[..first], false)?;
        let mut named: Vec<Target> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for node in call.args[first..].iter().flatten() {
            let Node::NamedArgExpr(arg) = node else {
                let positional = self.expr(node)?;
                let error = Error::parser("positional argument cannot follow named argument")
                    .state(SqlState::SYNTAX_ERROR)
                    .with_span(self.ast.expr_span(positional));
                return Err(error.into());
            };
            let text = arg.name.as_deref().unwrap_or_default();
            let Some(value) = &arg.arg else { return Err(not_yet(node)) };
            let expr = self.expr(value)?;
            if seen.contains(&text) {
                let message = format!("argument name \"{text}\" used more than once");
                let error = Error::parser(message)
                    .state(SqlState::SYNTAX_ERROR)
                    .with_span(self.at(arg.location));
                return Err(error.into());
            }
            seen.push(text);
            named.push(Target { expr, alias: self.intern(text) });
        }
        let filter = NONE;
        let made = self.push(Expr::Function { name, args, distinct: false, filter }, call.location);
        if !named.is_empty() {
            let named = self.ast.target_slice(named);
            self.ast.named_args.push((made, named));
        }
        if call.func_variadic {
            self.ast.variadic_calls.push(made);
        }
        Ok(made)
    }

    /// A call that the grammar made for a form of SQL syntax, with the name in `pg_catalog`.
    fn syntax_call(&mut self, call: &FuncCall) -> Made<ExprRef> {
        let location = call.location;
        let name = match Self::strings(&call.funcname)?[..] {
            ["pg_catalog", name] => name,
            _ => return clause("FuncCall"),
        };
        match (name, &call.args[..]) {
            (
                "substring" | "position" | "overlay" | "ltrim" | "rtrim" | "normalize"
                | "is_normalized",
                _,
            ) => self.call(name, &call.args, location),
            ("btrim", _) => self.call("trim", &call.args, location),
            ("extract", [Some(Node::A_Const(part)), Some(value)]) => {
                let Some(Node::String(written)) = &part.val else {
                    return clause("Extract");
                };
                // A part written as a word is spelled the way the DuckDB transform spells it, and a
                // part written as a string is kept as it is.
                let quoted =
                    self.token_at(part.location).map(|(kind, _)| kind) == Some(token::SCONST);
                let text = if quoted {
                    written.to_string()
                } else {
                    rudb_parse::build::date_part(written)
                };
                let part = self.string(&text, part.location);
                let value = self.expr(value)?;
                let args = self.ast.expr_slice([part, value]);
                Ok(self.made_call("date_part", args, location))
            }
            ("timezone", [Some(zone), Some(value)]) => {
                let zone = self.expr(zone)?;
                let value = self.expr(value)?;
                Ok(self.binary(BinaryOp::AtTimeZone, value, zone, location))
            }
            _ => clause("FuncCall"),
        }
    }
}

/// The arguments of the call to `pg_catalog.<name>` that the grammar puts on the right of `LIKE`,
/// `ILIKE` and `SIMILAR TO` for an `ESCAPE`, or for no escape in the case of `SIMILAR TO`.
fn escape_call<'n, const N: usize>(node: &'n Node, name: &str) -> Option<[&'n Node; N]> {
    let Node::FuncCall(call) = node else {
        return None;
    };
    let named = matches!(
        &call.funcname[..],
        [Some(Node::String(schema)), Some(Node::String(called))]
            if &**schema == "pg_catalog" && &**called == name
    );
    if !named || call.args.len() != N {
        return None;
    }
    let args: Vec<&Node> = call.args.iter().flatten().collect();
    args.try_into().ok()
}
