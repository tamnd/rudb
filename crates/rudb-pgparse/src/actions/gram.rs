//! The static functions of the prologue and the epilogue of `gram.y`, with the names and the
//! arguments of C. A function that takes `yyscanner` in C takes the [`Parser`] here, for its
//! errors.

use super::Parser;
use super::funcs::listLocation;
use super::make::*;
use crate::error::Error;
use crate::generated::glue::SelectLimit;
use crate::nodes::*;

impl Parser<'_> {
    /// `ereport(ERROR, errcode(code), errmsg(message), parser_errposition(location))`.
    pub(crate) fn error(&self, code: &'static str, message: &str, location: i32) -> Error {
        Error {
            code,
            message: message.to_owned(),
            hint: None,
            location: usize::try_from(location).ok(),
        }
    }
}

/// `strVal`: the text of a `String` node.
pub(crate) fn strVal(node: Option<&Node>) -> Option<Str> {
    match node {
        Some(Node::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `NameListToString` of `namespace.c`: the names joined with dots, and `*` for `A_Star`.
pub(crate) fn NameListToString(names: &List) -> String {
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        match name {
            Some(Node::String(s)) => out.push_str(s),
            Some(Node::A_Star(_)) => out.push('*'),
            _ => {}
        }
    }
    out
}

/// `makeRawStmt`.
pub(crate) fn makeRawStmt(stmt: Option<Node>, stmt_location: i32) -> RawStmt {
    // `stmt_len` can change later.
    RawStmt { stmt, stmt_location, stmt_len: 0 }
}

/// `updateRawStmtEnd`: the statement does not run to the end of the text.
pub(crate) fn updateRawStmtEnd(rs: &mut RawStmt, end_location: i32) {
    // A length that is set stays, for a text such as `select foo ;; select bar`, where the same
    // statement is the last one for more than one semicolon.
    if rs.stmt_len > 0 {
        return;
    }
    rs.stmt_len = end_location - rs.stmt_location;
}

/// `makeColumnRef`: a `ColumnRef`, in an `A_Indirection` when the indirection has a subscript.
/// A field selection at the start of the indirection goes into the fields of the `ColumnRef`.
pub(crate) fn makeColumnRef(
    colname: Option<Str>,
    mut indirection: List,
    location: i32,
    yyscanner: &Parser<'_>,
) -> Result<Node, Error> {
    let mut c = ColumnRef { fields: List::new(), location };
    let length = indirection.len();
    // The loop stops at the first subscript, so `nfields` is the number of field selections
    // before it.
    for (nfields, item) in indirection.iter().enumerate() {
        if matches!(item, Some(Node::A_Indices(_))) {
            let mut i = A_Indirection::default();
            if nfields == 0 {
                // All the indirection goes to the `A_Indirection`.
                c.fields = vec![Some(makeString(colname))];
                i.indirection = check_indirection(indirection, yyscanner)?;
            } else {
                // Split the list in two.
                let tail = indirection.split_off(nfields);
                i.indirection = check_indirection(tail, yyscanner)?;
                c.fields = indirection;
                c.fields.insert(0, Some(makeString(colname)));
            }
            i.arg = Some(c.into());
            return Ok(i.into());
        } else if matches!(item, Some(Node::A_Star(_))) {
            // `*` is only at the end of a `ColumnRef`.
            if nfields + 1 < length {
                return Err(yyscanner.yyerror("improper use of \"*\""));
            }
        }
    }
    // No subscript, so all the indirection goes to the fields.
    c.fields = indirection;
    c.fields.insert(0, Some(makeString(colname)));
    Ok(c.into())
}

/// `makeTypeCast`.
pub(crate) fn makeTypeCast(
    arg: Option<Node>,
    typename: Option<Box<TypeName>>,
    location: i32,
) -> Node {
    TypeCast { arg, typeName: typename, location }.into()
}

/// `makeStringConstCast`.
pub(crate) fn makeStringConstCast(
    str: Option<Str>,
    location: i32,
    typename: Option<Box<TypeName>>,
) -> Node {
    let s = makeStringConst(str, location);
    makeTypeCast(Some(s), typename, -1)
}

/// `makeIntConst`.
pub(crate) fn makeIntConst(val: i32, location: i32) -> Node {
    A_Const { val: Some(makeInteger(val)), isnull: false, location }.into()
}

/// `makeFloatConst`.
pub(crate) fn makeFloatConst(str: Option<Str>, location: i32) -> Node {
    A_Const { val: Some(makeFloat(str)), isnull: false, location }.into()
}

/// `makeBoolAConst`.
pub(crate) fn makeBoolAConst(state: bool, location: i32) -> Node {
    A_Const { val: Some(makeBoolean(state)), isnull: false, location }.into()
}

/// `makeBitStringConst`.
pub(crate) fn makeBitStringConst(str: Option<Str>, location: i32) -> Node {
    A_Const { val: Some(makeBitString(str)), isnull: false, location }.into()
}

/// `makeNullAConst`.
pub(crate) fn makeNullAConst(location: i32) -> Node {
    A_Const { val: None, isnull: true, location }.into()
}

/// `makeRoleSpec`.
pub(crate) fn makeRoleSpec(r#type: RoleSpecType, location: i32) -> RoleSpec {
    RoleSpec { roletype: r#type, location, ..RoleSpec::default() }
}

/// `check_qualified_name`: the `qualified_name` rule lets subscripts and `*` through, and this
/// rejects them.
pub(crate) fn check_qualified_name(names: &List, yyscanner: &Parser<'_>) -> Result<(), Error> {
    if names.iter().any(|name| !matches!(name, Some(Node::String(_)))) {
        return Err(yyscanner.yyerror("syntax error"));
    }
    Ok(())
}

/// `check_func_name`: the `func_name` rule lets subscripts and `*` through, and this rejects
/// them.
pub(crate) fn check_func_name(names: List, yyscanner: &Parser<'_>) -> Result<List, Error> {
    check_qualified_name(&names, yyscanner)?;
    Ok(names)
}

/// `check_indirection`: `*` is only at the end of the list.
pub(crate) fn check_indirection(indirection: List, yyscanner: &Parser<'_>) -> Result<List, Error> {
    let length = indirection.len();
    for (l, item) in indirection.iter().enumerate() {
        if matches!(item, Some(Node::A_Star(_))) && l + 1 < length {
            return Err(yyscanner.yyerror("improper use of \"*\""));
        }
    }
    Ok(indirection)
}

/// `insertSelectOptions`: puts `ORDER BY` and the other options into a `SelectStmt`.
pub(crate) fn insertSelectOptions(
    stmt: &mut SelectStmt,
    sortClause: List,
    lockingClause: List,
    limitClause: Option<Box<SelectLimit>>,
    withClause: Option<Box<WithClause>>,
    yyscanner: &Parser<'_>,
) -> Result<(), Error> {
    // The tests reject a statement such as `(SELECT foo ORDER BY bar) ORDER BY baz`.
    if !sortClause.is_empty() {
        if !stmt.sortClause.is_empty() {
            let location = listLocation(&sortClause);
            return Err(yyscanner.error(
                ERRCODE_SYNTAX_ERROR,
                "multiple ORDER BY clauses not allowed",
                location,
            ));
        }
        stmt.sortClause = sortClause;
    }
    // More than one locking clause is correct.
    stmt.lockingClause.extend(lockingClause);
    if let Some(mut limitClause) = limitClause {
        if let Some(limitOffset) = limitClause.limitOffset.take() {
            if stmt.limitOffset.is_some() {
                let location = limitClause.offsetLoc;
                return Err(yyscanner.error(
                    ERRCODE_SYNTAX_ERROR,
                    "multiple OFFSET clauses not allowed",
                    location,
                ));
            }
            stmt.limitOffset = Some(limitOffset);
        }
        if let Some(limitCount) = limitClause.limitCount.take() {
            if stmt.limitCount.is_some() {
                let location = limitClause.countLoc;
                return Err(yyscanner.error(
                    ERRCODE_SYNTAX_ERROR,
                    "multiple LIMIT clauses not allowed",
                    location,
                ));
            }
            stmt.limitCount = Some(limitCount);
        }
        let with_ties = limitClause.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES;
        if stmt.sortClause.is_empty() && with_ties {
            let message = "WITH TIES cannot be specified without ORDER BY clause";
            return Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, message, limitClause.optionLoc));
        }
        if with_ties {
            for lock in &stmt.lockingClause {
                if let Some(Node::LockingClause(lock)) = lock
                    && lock.waitPolicy == LockWaitPolicy::LockWaitSkip
                {
                    let message = "SKIP LOCKED and WITH TIES options cannot be used together";
                    return Err(yyscanner.error(
                        ERRCODE_SYNTAX_ERROR,
                        message,
                        limitClause.optionLoc,
                    ));
                }
            }
        }
        stmt.limitOption = limitClause.limitOption;
    }
    if let Some(withClause) = withClause {
        if stmt.withClause.is_some() {
            let location = withClause.location;
            return Err(yyscanner.error(
                ERRCODE_SYNTAX_ERROR,
                "multiple WITH clauses not allowed",
                location,
            ));
        }
        stmt.withClause = Some(withClause);
    }
    Ok(())
}

/// `makeSetOp`.
pub(crate) fn makeSetOp(
    op: SetOperation,
    all: bool,
    larg: Option<Node>,
    rarg: Option<Node>,
) -> Node {
    let select = |node: Option<Node>| node.and_then(|node| SelectStmt::from_node(node).ok());
    SelectStmt { op, all, larg: select(larg), rarg: select(rarg), ..SelectStmt::default() }.into()
}

/// `SystemFuncName`: the name of a built-in function, in `pg_catalog`.
pub(crate) fn SystemFuncName(name: &str) -> List {
    vec![Some(makeString(Some("pg_catalog".into()))), Some(makeString(Some(name.into())))]
}

/// `SystemTypeName`: the name of a built-in type, in `pg_catalog`.
pub(crate) fn SystemTypeName(name: &str) -> TypeName {
    makeTypeNameFromNameList(SystemFuncName(name))
}

/// `doNegate`: the negation of a number constant is a constant, so that `-123.456` stays a
/// string until the type is known. The location of the constant becomes that of the `-`.
pub(crate) fn doNegate(n: Option<Node>, location: i32) -> Node {
    match n {
        Some(Node::A_Const(mut con))
            if matches!(con.val, Some(Node::Integer(_) | Node::Float(_))) =>
        {
            con.location = location;
            match &mut con.val {
                Some(Node::Integer(ival)) => *ival = ival.wrapping_neg(),
                Some(Node::Float(fval)) => doNegateFloat(fval),
                _ => {}
            }
            Node::A_Const(con)
        }
        Some(Node::A_Const(mut con)) => {
            con.location = location;
            makeSimpleA_Expr(A_Expr_Kind::AEXPR_OP, "-", None, Some(Node::A_Const(con)), location)
                .into()
        }
        n => makeSimpleA_Expr(A_Expr_Kind::AEXPR_OP, "-", None, n, location).into(),
    }
}

/// `doNegateFloat`.
pub(crate) fn doNegateFloat(v: &mut Str) {
    let oldval = v.strip_prefix('+').unwrap_or(v);
    *v = match oldval.strip_prefix('-') {
        // Remove the `-`.
        Some(rest) => rest.into(),
        None => format!("-{oldval}").into(),
    };
}

/// `makeAndExpr`: `a AND b AND c` is one `BoolExpr`.
pub(crate) fn makeAndExpr(lexpr: Option<Node>, rexpr: Option<Node>, location: i32) -> Node {
    match lexpr {
        Some(Node::BoolExpr(mut blexpr)) if blexpr.boolop == BoolExprType::AND_EXPR => {
            blexpr.args.push(rexpr);
            Node::BoolExpr(blexpr)
        }
        lexpr => makeBoolExpr(BoolExprType::AND_EXPR, vec![lexpr, rexpr], location).into(),
    }
}

/// `makeOrExpr`: `a OR b OR c` is one `BoolExpr`.
pub(crate) fn makeOrExpr(lexpr: Option<Node>, rexpr: Option<Node>, location: i32) -> Node {
    match lexpr {
        Some(Node::BoolExpr(mut blexpr)) if blexpr.boolop == BoolExprType::OR_EXPR => {
            blexpr.args.push(rexpr);
            Node::BoolExpr(blexpr)
        }
        lexpr => makeBoolExpr(BoolExprType::OR_EXPR, vec![lexpr, rexpr], location).into(),
    }
}

/// `makeNotExpr`.
pub(crate) fn makeNotExpr(expr: Option<Node>, location: i32) -> Node {
    makeBoolExpr(BoolExprType::NOT_EXPR, vec![expr], location).into()
}

/// `makeAArrayExpr`.
pub(crate) fn makeAArrayExpr(elements: List, location: i32, location_end: i32) -> Node {
    A_ArrayExpr { elements, location, list_start: location, list_end: location_end }.into()
}

/// `makeSQLValueFunction`. The type comes later, in the analysis.
pub(crate) fn makeSQLValueFunction(op: SQLValueFunctionOp, typmod: i32, location: i32) -> Node {
    SQLValueFunction { op, typmod, location, ..SQLValueFunction::default() }.into()
}

/// `makeXmlExpr`. The analysis splits the `ResTarget` list `named_args` into the names and the
/// expressions, and sets the type. The caller sets `xmloption` where it applies.
pub(crate) fn makeXmlExpr(
    op: XmlExprOp,
    name: Option<Str>,
    named_args: List,
    args: List,
    location: i32,
) -> XmlExpr {
    XmlExpr { op, name, named_args, args, location, ..XmlExpr::default() }
}

/// `makeRangeVarFromQualifiedName`: a `RangeVar` from the name and the names after it of the
/// `relation_name` rule.
pub(crate) fn makeRangeVarFromQualifiedName(
    name: Option<Str>,
    namelist: List,
    location: i32,
    yyscanner: &Parser<'_>,
) -> Result<RangeVar, Error> {
    check_qualified_name(&namelist, yyscanner)?;
    let mut r = makeRangeVar(None, None, location);
    match namelist.as_slice() {
        [relname] => {
            r.schemaname = name;
            r.relname = strVal(relname.as_ref());
        }
        [schemaname, relname] => {
            r.catalogname = name;
            r.schemaname = strVal(schemaname.as_ref());
            r.relname = strVal(relname.as_ref());
        }
        _ => {
            let mut names = namelist;
            names.insert(0, Some(makeString(name)));
            let message = format!(
                "improper qualified name (too many dotted names): {}",
                NameListToString(&names)
            );
            return Err(yyscanner.error(ERRCODE_SYNTAX_ERROR, &message, location));
        }
    }
    Ok(r)
}
