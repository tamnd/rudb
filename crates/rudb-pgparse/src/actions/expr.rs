//! The actions of the expressions: `a_expr`, `b_expr` and `c_expr`, the function calls and the
//! functions with a special syntax, the window clauses, `CASE`, the arrays, the column references
//! and the subscripts, the XML expressions, and the SQL/JSON expressions with the functions of
//! `json.rs`.

use super::json::{behaviors, is_json, query_function};
use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

/// `(Node *) makeSimpleA_Expr(AEXPR_OP, name, lexpr, rexpr, location)`.
fn op(
    name: &str,
    lexpr: Option<Node>,
    rexpr: Option<Node>,
    location: i32,
) -> Result<Option<Node>, Error> {
    Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_OP, name, lexpr, rexpr, location).into()))
}

/// `(Node *) makeA_Expr(AEXPR_OP, name, lexpr, rexpr, location)`, for a qualified operator.
fn qual_op(
    name: List,
    lexpr: Option<Node>,
    rexpr: Option<Node>,
    location: i32,
) -> Result<Option<Node>, Error> {
    Ok(Some(makeA_Expr(A_Expr_Kind::AEXPR_OP, name, lexpr, rexpr, location).into()))
}

/// `(Node *) makeFuncCall(SystemFuncName(name), args, COERCE_SQL_SYNTAX, location)`, a built-in
/// function that has a special syntax.
fn sql_syntax(name: &str, args: List, location: i32) -> Node {
    makeFuncCall(SystemFuncName(name), args, CoercionForm::COERCE_SQL_SYNTAX, location).into()
}

/// A `LIKE`, `ILIKE` or `SIMILAR TO` with an escape or with the escape function of `SIMILAR TO`:
/// the pattern is a call of `function` with `args`.
fn like_escape(
    kind: A_Expr_Kind,
    name: &str,
    lexpr: Option<Node>,
    function: &str,
    args: List,
    location: i32,
) -> Result<Option<Node>, Error> {
    let n =
        makeFuncCall(SystemFuncName(function), args, CoercionForm::COERCE_EXPLICIT_CALL, location);
    Ok(Some(makeSimpleA_Expr(kind, name, lexpr, Some(n.into()), location).into()))
}

/// A `NullTest` of `arg`.
fn null_test(
    arg: Option<Node>,
    nulltesttype: NullTestType,
    location: i32,
) -> Result<Option<Node>, Error> {
    Ok(Some(NullTest { arg, nulltesttype, location, ..NullTest::default() }.into()))
}

/// A `BooleanTest` of `arg`.
fn boolean_test(
    arg: Option<Node>,
    booltesttype: BoolTestType,
    location: i32,
) -> Result<Option<Node>, Error> {
    Ok(Some(BooleanTest { arg, booltesttype, location }.into()))
}

/// A `BETWEEN` of the kind `kind`, with the bounds as a list in `rexpr`.
fn between(
    kind: A_Expr_Kind,
    name: &str,
    lexpr: Option<Node>,
    lower: Option<Node>,
    upper: Option<Node>,
    location: i32,
) -> Result<Option<Node>, Error> {
    let bounds = Some(Node::List(list_make2(lower, upper)));
    Ok(Some(makeSimpleA_Expr(kind, name, lexpr, bounds, location).into()))
}

/// A `SubLink` with the ID 0, as the grammar makes them.
fn sublink(
    subLinkType: SubLinkType,
    testexpr: Option<Node>,
    operName: List,
    subselect: Option<Node>,
    location: i32,
) -> SubLink {
    SubLink { subLinkType, subLinkId: 0, testexpr, operName, subselect, location }
}

/// `IN (list)` and `NOT IN (list)`: an `AEXPR_IN` with the locations of the parentheses.
fn in_list(
    name: &str,
    lexpr: Option<Node>,
    list: List,
    location: i32,
    start: i32,
    end: i32,
) -> Result<Option<Node>, Error> {
    let mut n =
        makeSimpleA_Expr(A_Expr_Kind::AEXPR_IN, name, lexpr, Some(Node::List(list)), location);
    n.rexpr_list_start = start;
    n.rexpr_list_end = end;
    Ok(Some(n.into()))
}

/// `$1 IS [NOT] [form] NORMALIZED`: a call of `is_normalized`.
fn is_normalized(arg: Option<Node>, form: Option<(Option<Str>, i32)>, location: i32) -> Node {
    let args = match form {
        Some((form, at)) => list_make2(arg, Some(makeStringConst(form, at))),
        None => list_make1(arg),
    };
    sql_syntax("is_normalized", args, location)
}

/// `$1 IS DOCUMENT`.
fn is_document(arg: Option<Node>, location: i32) -> Node {
    makeXmlExpr(XmlExprOp::IS_DOCUMENT, None, List::new(), list_make1(arg), location).into()
}

impl rules::a_expr for Parser<'_> {
    fn a_expr_2(
        &mut self,
        v1: Option<Node>,
        v3: Option<Box<TypeName>>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeTypeCast(v1, v3, at2)))
    }

    fn a_expr_3(&mut self, v1: Option<Node>, v3: List, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(CollateClause { arg: v1, collname: v3, location: at2 }.into()))
    }

    fn a_expr_4(
        &mut self,
        v1: Option<Node>,
        v5: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("timezone", list_make2(v5, v1), at2)))
    }

    fn a_expr_5(&mut self, v1: Option<Node>) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("timezone", list_make1(v1), -1)))
    }

    fn a_expr_6(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        op("+", None, v2, at1)
    }

    fn a_expr_7(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(doNegate(v2, at1)))
    }

    fn a_expr_8(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("+", v1, v3, at2)
    }

    fn a_expr_9(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("-", v1, v3, at2)
    }

    fn a_expr_10(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("*", v1, v3, at2)
    }

    fn a_expr_11(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("/", v1, v3, at2)
    }

    fn a_expr_12(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("%", v1, v3, at2)
    }

    fn a_expr_13(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("^", v1, v3, at2)
    }

    fn a_expr_14(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("<", v1, v3, at2)
    }

    fn a_expr_15(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op(">", v1, v3, at2)
    }

    fn a_expr_16(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("=", v1, v3, at2)
    }

    fn a_expr_17(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("<=", v1, v3, at2)
    }

    fn a_expr_18(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op(">=", v1, v3, at2)
    }

    fn a_expr_19(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("<>", v1, v3, at2)
    }

    fn a_expr_20(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        qual_op(v2, v1, v3, at2)
    }

    fn a_expr_21(&mut self, v1: List, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        qual_op(v1, None, v2, at1)
    }

    fn a_expr_22(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeAndExpr(v1, v3, at2)))
    }

    fn a_expr_23(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeOrExpr(v1, v3, at2)))
    }

    fn a_expr_24(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeNotExpr(v2, at1)))
    }

    fn a_expr_25(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeNotExpr(v2, at1)))
    }

    fn a_expr_26(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_LIKE, "~~", v1, v3, at2).into()))
    }

    fn a_expr_27(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v5: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(A_Expr_Kind::AEXPR_LIKE, "~~", v1, "like_escape", list_make2(v3, v5), at2)
    }

    fn a_expr_28(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_LIKE, "!~~", v1, v4, at2).into()))
    }

    fn a_expr_29(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(A_Expr_Kind::AEXPR_LIKE, "!~~", v1, "like_escape", list_make2(v4, v6), at2)
    }

    fn a_expr_30(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_ILIKE, "~~*", v1, v3, at2).into()))
    }

    fn a_expr_31(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v5: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(A_Expr_Kind::AEXPR_ILIKE, "~~*", v1, "like_escape", list_make2(v3, v5), at2)
    }

    fn a_expr_32(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_ILIKE, "!~~*", v1, v4, at2).into()))
    }

    fn a_expr_33(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(A_Expr_Kind::AEXPR_ILIKE, "!~~*", v1, "like_escape", list_make2(v4, v6), at2)
    }

    fn a_expr_34(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(A_Expr_Kind::AEXPR_SIMILAR, "~", v1, "similar_to_escape", list_make1(v4), at2)
    }

    fn a_expr_35(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(
            A_Expr_Kind::AEXPR_SIMILAR,
            "~",
            v1,
            "similar_to_escape",
            list_make2(v4, v6),
            at2,
        )
    }

    fn a_expr_36(
        &mut self,
        v1: Option<Node>,
        v5: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(A_Expr_Kind::AEXPR_SIMILAR, "!~", v1, "similar_to_escape", list_make1(v5), at2)
    }

    fn a_expr_37(
        &mut self,
        v1: Option<Node>,
        v5: Option<Node>,
        v7: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        like_escape(
            A_Expr_Kind::AEXPR_SIMILAR,
            "!~",
            v1,
            "similar_to_escape",
            list_make2(v5, v7),
            at2,
        )
    }

    fn a_expr_38(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NULL, at2)
    }

    fn a_expr_39(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NULL, at2)
    }

    fn a_expr_40(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NOT_NULL, at2)
    }

    fn a_expr_41(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NOT_NULL, at2)
    }

    fn a_expr_42(
        &mut self,
        v1: List,
        v3: List,
        at1: i32,
        at2: i32,
        at3: i32,
    ) -> Result<Option<Node>, Error> {
        if v1.len() != 2 {
            let message = "wrong number of parameters on left side of OVERLAPS expression";
            return Err(self.error(ERRCODE_SYNTAX_ERROR, message, at1));
        }
        if v3.len() != 2 {
            let message = "wrong number of parameters on right side of OVERLAPS expression";
            return Err(self.error(ERRCODE_SYNTAX_ERROR, message, at3));
        }
        Ok(Some(sql_syntax("overlaps", list_concat(v1, v3), at2)))
    }

    fn a_expr_43(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_TRUE, at2)
    }

    fn a_expr_44(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_NOT_TRUE, at2)
    }

    fn a_expr_45(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_FALSE, at2)
    }

    fn a_expr_46(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_NOT_FALSE, at2)
    }

    fn a_expr_47(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_UNKNOWN, at2)
    }

    fn a_expr_48(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_NOT_UNKNOWN, at2)
    }

    fn a_expr_49(
        &mut self,
        v1: Option<Node>,
        v5: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_DISTINCT, "=", v1, v5, at2).into()))
    }

    fn a_expr_50(
        &mut self,
        v1: Option<Node>,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_NOT_DISTINCT, "=", v1, v6, at2).into()))
    }

    fn a_expr_51(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        between(A_Expr_Kind::AEXPR_BETWEEN, "BETWEEN", v1, v4, v6, at2)
    }

    fn a_expr_52(
        &mut self,
        v1: Option<Node>,
        v5: Option<Node>,
        v7: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        between(A_Expr_Kind::AEXPR_NOT_BETWEEN, "NOT BETWEEN", v1, v5, v7, at2)
    }

    fn a_expr_53(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        between(A_Expr_Kind::AEXPR_BETWEEN_SYM, "BETWEEN SYMMETRIC", v1, v4, v6, at2)
    }

    fn a_expr_54(
        &mut self,
        v1: Option<Node>,
        v5: Option<Node>,
        v7: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        between(A_Expr_Kind::AEXPR_NOT_BETWEEN_SYM, "NOT BETWEEN SYMMETRIC", v1, v5, v7, at2)
    }

    fn a_expr_55(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        // `foo = ANY (subquery)`. An empty `operName` shows that it is `IN` and not `= ANY`.
        Ok(Some(sublink(SubLinkType::ANY_SUBLINK, v1, List::new(), v3, at2).into()))
    }

    fn a_expr_56(
        &mut self,
        v1: Option<Node>,
        v4: List,
        at2: i32,
        at3: i32,
        at5: i32,
    ) -> Result<Option<Node>, Error> {
        in_list("=", v1, v4, at2, at3, at5)
    }

    fn a_expr_57(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        // `NOT (foo = ANY (subquery))`, with the same location for the `NOT`.
        let n = sublink(SubLinkType::ANY_SUBLINK, v1, List::new(), v4, at2);
        Ok(Some(makeNotExpr(Some(n.into()), at2)))
    }

    fn a_expr_58(
        &mut self,
        v1: Option<Node>,
        v5: List,
        at2: i32,
        at4: i32,
        at6: i32,
    ) -> Result<Option<Node>, Error> {
        in_list("<>", v1, v5, at2, at4, at6)
    }

    fn a_expr_59(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: i32,
        v4: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(sublink(SubLinkType(v3), v1, v2, v4, at2).into()))
    }

    fn a_expr_60(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: i32,
        v5: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        let kind = if SubLinkType(v3) == SubLinkType::ANY_SUBLINK {
            A_Expr_Kind::AEXPR_OP_ANY
        } else {
            A_Expr_Kind::AEXPR_OP_ALL
        };
        Ok(Some(makeA_Expr(kind, v2, v1, v5, at2).into()))
    }

    fn a_expr_61(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        let message = "UNIQUE predicate is not yet implemented";
        Err(self.error(ERRCODE_FEATURE_NOT_SUPPORTED, message, at1))
    }

    fn a_expr_62(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(is_document(v1, at2)))
    }

    fn a_expr_63(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeNotExpr(Some(is_document(v1, at2)), at2)))
    }

    fn a_expr_64(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(is_normalized(v1, None, at2)))
    }

    fn a_expr_65(
        &mut self,
        v1: Option<Node>,
        v3: Option<Str>,
        at2: i32,
        at3: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(is_normalized(v1, Some((v3, at3)), at2)))
    }

    fn a_expr_66(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeNotExpr(Some(is_normalized(v1, None, at2)), at2)))
    }

    fn a_expr_67(
        &mut self,
        v1: Option<Node>,
        v4: Option<Str>,
        at2: i32,
        at4: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeNotExpr(Some(is_normalized(v1, Some((v4, at4)), at2)), at2)))
    }

    fn a_expr_68(
        &mut self,
        v1: Option<Node>,
        v3: i32,
        v4: bool,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(is_json(v1, v3, v4, at1)))
    }

    fn a_expr_69(
        &mut self,
        v1: Option<Node>,
        v4: i32,
        v5: bool,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeNotExpr(Some(is_json(v1, v4, v5, at1)), at1)))
    }

    fn a_expr_70(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        // `DEFAULT` can be any `a_expr` here, and the analysis gives an error where it is not
        // allowed. The analysis also sets the other fields.
        Ok(Some(SetToDefault { location: at1, ..SetToDefault::default() }.into()))
    }
}

impl rules::b_expr for Parser<'_> {
    fn b_expr_2(
        &mut self,
        v1: Option<Node>,
        v3: Option<Box<TypeName>>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeTypeCast(v1, v3, at2)))
    }

    fn b_expr_3(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        op("+", None, v2, at1)
    }

    fn b_expr_4(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(doNegate(v2, at1)))
    }

    fn b_expr_5(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("+", v1, v3, at2)
    }

    fn b_expr_6(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("-", v1, v3, at2)
    }

    fn b_expr_7(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("*", v1, v3, at2)
    }

    fn b_expr_8(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("/", v1, v3, at2)
    }

    fn b_expr_9(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("%", v1, v3, at2)
    }

    fn b_expr_10(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("^", v1, v3, at2)
    }

    fn b_expr_11(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("<", v1, v3, at2)
    }

    fn b_expr_12(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op(">", v1, v3, at2)
    }

    fn b_expr_13(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("=", v1, v3, at2)
    }

    fn b_expr_14(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("<=", v1, v3, at2)
    }

    fn b_expr_15(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op(">=", v1, v3, at2)
    }

    fn b_expr_16(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        op("<>", v1, v3, at2)
    }

    fn b_expr_17(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        qual_op(v2, v1, v3, at2)
    }

    fn b_expr_18(&mut self, v1: List, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        qual_op(v1, None, v2, at1)
    }

    fn b_expr_19(
        &mut self,
        v1: Option<Node>,
        v5: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_DISTINCT, "=", v1, v5, at2).into()))
    }

    fn b_expr_20(
        &mut self,
        v1: Option<Node>,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_NOT_DISTINCT, "=", v1, v6, at2).into()))
    }

    fn b_expr_21(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(is_document(v1, at2)))
    }

    fn b_expr_22(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeNotExpr(Some(is_document(v1, at2)), at2)))
    }
}

/// `arg` with the subscripts and the field selections `indirection`, or `arg` alone when there
/// are none.
fn indirect(
    arg: Option<Node>,
    indirection: List,
    yyscanner: &Parser<'_>,
) -> Result<Option<Node>, Error> {
    if indirection.is_empty() {
        return Ok(arg);
    }
    let indirection = check_indirection(indirection, yyscanner)?;
    Ok(Some(A_Indirection { arg, indirection }.into()))
}

/// A `RowExpr` of `args`. The analysis sets the type and the column names. The format tells an
/// explicit `ROW(...)` from a row with no keyword.
fn row(args: List, row_format: CoercionForm, location: i32) -> Result<Option<Node>, Error> {
    Ok(Some(RowExpr { args, row_format, location, ..RowExpr::default() }.into()))
}

impl rules::c_expr for Parser<'_> {
    fn c_expr_3(&mut self, v1: i32, v2: List, at1: i32) -> Result<Option<Node>, Error> {
        let p = ParamRef { number: v1, location: at1 };
        indirect(Some(p.into()), v2, self)
    }

    fn c_expr_4(&mut self, v2: Option<Node>, v4: List) -> Result<Option<Node>, Error> {
        indirect(v2, v4, self)
    }

    fn c_expr_7(&mut self, v1: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sublink(SubLinkType::EXPR_SUBLINK, None, List::new(), v1, at1).into()))
    }

    fn c_expr_8(&mut self, v1: Option<Node>, v2: List, at1: i32) -> Result<Option<Node>, Error> {
        // The `'(' a_expr ')' opt_indirection` rule does not take a sub-SELECT with subscripts or
        // field selections, because `select_with_parens` takes all the parentheses. This rule
        // takes them.
        let n = sublink(SubLinkType::EXPR_SUBLINK, None, List::new(), v1, at1);
        let indirection = check_indirection(v2, self)?;
        Ok(Some(A_Indirection { arg: Some(n.into()), indirection }.into()))
    }

    fn c_expr_9(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sublink(SubLinkType::EXISTS_SUBLINK, None, List::new(), v2, at1).into()))
    }

    fn c_expr_10(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sublink(SubLinkType::ARRAY_SUBLINK, None, List::new(), v2, at1).into()))
    }

    fn c_expr_11(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        // The location of the outermost `A_ArrayExpr` is that of the `ARRAY` keyword.
        let n = change(castNode::<A_ArrayExpr>(v2)?, |n| n.location = at1);
        Ok(n.map(NodeType::into_node))
    }

    fn c_expr_12(&mut self, v1: List, at1: i32) -> Result<Option<Node>, Error> {
        row(v1, CoercionForm::COERCE_EXPLICIT_CALL, at1)
    }

    fn c_expr_13(&mut self, v1: List, at1: i32) -> Result<Option<Node>, Error> {
        row(v1, CoercionForm::COERCE_IMPLICIT_CAST, at1)
    }

    fn c_expr_14(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(GroupingFunc { args: v3, location: at1, ..GroupingFunc::default() }.into()))
    }
}

/// `makeFuncCall(name, args, COERCE_EXPLICIT_CALL, location)`, a plain function call.
fn call(name: List, args: List, location: i32) -> FuncCall {
    makeFuncCall(name, args, CoercionForm::COERCE_EXPLICIT_CALL, location)
}

impl rules::func_application for Parser<'_> {
    fn func_application_1(&mut self, v1: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(call(v1, List::new(), at1).into()))
    }

    fn func_application_2(
        &mut self,
        v1: List,
        v3: List,
        v4: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(FuncCall { agg_order: v4, ..call(v1, v3, at1) }.into()))
    }

    fn func_application_3(
        &mut self,
        v1: List,
        v4: Option<Node>,
        v5: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = call(v1, list_make1(v4), at1);
        Ok(Some(FuncCall { func_variadic: true, agg_order: v5, ..n }.into()))
    }

    fn func_application_4(
        &mut self,
        v1: List,
        v3: List,
        v6: Option<Node>,
        v7: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = call(v1, lappend(v3, v6), at1);
        Ok(Some(FuncCall { func_variadic: true, agg_order: v7, ..n }.into()))
    }

    fn func_application_5(
        &mut self,
        v1: List,
        v4: List,
        v5: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // `FuncCall` has no field to tell that the function must be an aggregate.
        Ok(Some(FuncCall { agg_order: v5, ..call(v1, v4, at1) }.into()))
    }

    fn func_application_6(
        &mut self,
        v1: List,
        v4: List,
        v5: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(FuncCall { agg_order: v5, agg_distinct: true, ..call(v1, v4, at1) }.into()))
    }

    fn func_application_7(&mut self, v1: List, at1: i32) -> Result<Option<Node>, Error> {
        // `AGGREGATE(*)` calls an aggregate with no arguments, as `COUNT(*)` does. `agg_star`
        // keeps the `*` for the analysis.
        Ok(Some(FuncCall { agg_star: true, ..call(v1, List::new(), at1) }.into()))
    }
}

impl rules::func_expr for Parser<'_> {
    fn func_expr_1(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: Option<Node>,
        v4: i32,
        v5: Option<Box<WindowDef>>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        let n: &mut FuncCall = castMut(&mut v1)?;
        // `WITHIN GROUP` and the `ORDER BY` of an aggregate use the same field, so only one of
        // them can be there. The checks of `DISTINCT` and `VARIADIC` are here to give a better
        // location. The analysis does the other checks.
        if !v2.is_empty() {
            let message = if !n.agg_order.is_empty() {
                Some("cannot use multiple ORDER BY clauses with WITHIN GROUP")
            } else if n.agg_distinct {
                Some("cannot use DISTINCT with WITHIN GROUP")
            } else if n.func_variadic {
                Some("cannot use VARIADIC with WITHIN GROUP")
            } else {
                None
            };
            if let Some(message) = message {
                return Err(self.error(ERRCODE_SYNTAX_ERROR, message, at2));
            }
            n.agg_order = v2;
            n.agg_within_group = true;
        }
        n.agg_filter = v3;
        n.ignore_nulls = v4;
        n.over = v5;
        Ok(v1)
    }

    fn func_expr_2(
        &mut self,
        v1: Option<Node>,
        v2: Option<Node>,
        v3: Option<Box<WindowDef>>,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        let constructor = match v1.as_mut() {
            Some(Node::JsonObjectAgg(n)) => &mut n.constructor,
            Some(Node::JsonArrayAgg(n)) => &mut n.constructor,
            _ => return Err(Error::internal("a cast of a node of the wrong type")),
        };
        if let Some(n) = constructor {
            n.agg_filter = v2;
            n.over = v3;
        }
        Ok(v1)
    }
}

impl rules::func_expr_common_subexpr for Parser<'_> {
    fn func_expr_common_subexpr_1(
        &mut self,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("pg_collation_for", list_make1(v4), at1)))
    }

    fn func_expr_common_subexpr_2(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_DATE, -1, at1)))
    }

    fn func_expr_common_subexpr_3(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_TIME, -1, at1)))
    }

    fn func_expr_common_subexpr_4(&mut self, v3: i32, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_TIME_N, v3, at1)))
    }

    fn func_expr_common_subexpr_5(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP, -1, at1)))
    }

    fn func_expr_common_subexpr_6(&mut self, v3: i32, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP_N, v3, at1)))
    }

    fn func_expr_common_subexpr_7(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_LOCALTIME, -1, at1)))
    }

    fn func_expr_common_subexpr_8(&mut self, v3: i32, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_LOCALTIME_N, v3, at1)))
    }

    fn func_expr_common_subexpr_9(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP, -1, at1)))
    }

    fn func_expr_common_subexpr_10(&mut self, v3: i32, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP_N, v3, at1)))
    }

    fn func_expr_common_subexpr_11(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_ROLE, -1, at1)))
    }

    fn func_expr_common_subexpr_12(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_USER, -1, at1)))
    }

    fn func_expr_common_subexpr_13(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_SESSION_USER, -1, at1)))
    }

    fn func_expr_common_subexpr_14(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("system_user", List::new(), at1)))
    }

    fn func_expr_common_subexpr_15(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_USER, -1, at1)))
    }

    fn func_expr_common_subexpr_16(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_CATALOG, -1, at1)))
    }

    fn func_expr_common_subexpr_17(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeSQLValueFunction(SQLValueFunctionOp::SVFOP_CURRENT_SCHEMA, -1, at1)))
    }

    fn func_expr_common_subexpr_18(
        &mut self,
        v3: Option<Node>,
        v5: Option<Box<TypeName>>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeTypeCast(v3, v5, at1)))
    }

    fn func_expr_common_subexpr_19(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("extract", v3, at1)))
    }

    fn func_expr_common_subexpr_20(
        &mut self,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("normalize", list_make1(v3), at1)))
    }

    fn func_expr_common_subexpr_21(
        &mut self,
        v3: Option<Node>,
        v5: Option<Str>,
        at1: i32,
        at5: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("normalize", list_make2(v3, Some(makeStringConst(v5, at5))), at1)))
    }

    fn func_expr_common_subexpr_22(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("overlay", v3, at1)))
    }

    fn func_expr_common_subexpr_23(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        // A function named `overlay` with the plain call syntax.
        Ok(Some(call(list_make1(Some(makeString(Some("overlay".into())))), v3, at1).into()))
    }

    fn func_expr_common_subexpr_24(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        // `position(A in B)` is `position(B, A)`. There is no plain call syntax for `position`,
        // because the reversed arguments would be confusing.
        Ok(Some(sql_syntax("position", v3, at1)))
    }

    fn func_expr_common_subexpr_25(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        // `substring(A from B for C)` is `substring(A, B, C)`.
        Ok(Some(sql_syntax("substring", v3, at1)))
    }

    fn func_expr_common_subexpr_26(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        // A function named `substring` with the plain call syntax.
        Ok(Some(call(list_make1(Some(makeString(Some("substring".into())))), v3, at1).into()))
    }

    fn func_expr_common_subexpr_27(
        &mut self,
        v3: Option<Node>,
        v5: Option<Box<TypeName>>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // `TREAT(expr AS target)` is a call of the function with the name of the type, which
        // allows stronger coercions than the implicit casts.
        let name = v5.and_then(|t| strVal(t.names.last().and_then(Option::as_ref)));
        let name = SystemFuncName(name.as_deref().unwrap_or_default());
        Ok(Some(call(name, list_make1(v3), at1).into()))
    }

    fn func_expr_common_subexpr_28(&mut self, v4: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("btrim", v4, at1)))
    }

    fn func_expr_common_subexpr_29(&mut self, v4: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("ltrim", v4, at1)))
    }

    fn func_expr_common_subexpr_30(&mut self, v4: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("rtrim", v4, at1)))
    }

    fn func_expr_common_subexpr_31(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(sql_syntax("btrim", v3, at1)))
    }

    fn func_expr_common_subexpr_32(
        &mut self,
        v3: Option<Node>,
        v5: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_NULLIF, "=", v3, v5, at1).into()))
    }

    fn func_expr_common_subexpr_33(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(CoalesceExpr { args: v3, location: at1, ..CoalesceExpr::default() }.into()))
    }

    fn func_expr_common_subexpr_34(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        let v = MinMaxExpr {
            args: v3,
            op: MinMaxOp::IS_GREATEST,
            location: at1,
            ..MinMaxExpr::default()
        };
        Ok(Some(v.into()))
    }

    fn func_expr_common_subexpr_35(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        let v =
            MinMaxExpr { args: v3, op: MinMaxOp::IS_LEAST, location: at1, ..MinMaxExpr::default() };
        Ok(Some(v.into()))
    }

    fn func_expr_common_subexpr_36(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLCONCAT, None, List::new(), v3, at1).into()))
    }

    fn func_expr_common_subexpr_37(
        &mut self,
        v4: Option<Str>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLELEMENT, v4, List::new(), List::new(), at1).into()))
    }

    fn func_expr_common_subexpr_38(
        &mut self,
        v4: Option<Str>,
        v6: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLELEMENT, v4, v6, List::new(), at1).into()))
    }

    fn func_expr_common_subexpr_39(
        &mut self,
        v4: Option<Str>,
        v6: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLELEMENT, v4, List::new(), v6, at1).into()))
    }

    fn func_expr_common_subexpr_40(
        &mut self,
        v4: Option<Str>,
        v6: List,
        v8: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLELEMENT, v4, v6, v8, at1).into()))
    }

    fn func_expr_common_subexpr_41(
        &mut self,
        v3: Option<Node>,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // `xmlexists(A PASSING [BY REF] B [BY REF])` is `xmlexists(A, B)`.
        Ok(Some(sql_syntax("xmlexists", list_make2(v3, v4), at1)))
    }

    fn func_expr_common_subexpr_42(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLFOREST, None, v3, List::new(), at1).into()))
    }

    fn func_expr_common_subexpr_43(
        &mut self,
        v3: i32,
        v4: Option<Node>,
        v5: bool,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let args = list_make2(v4, Some(makeBoolAConst(v5, -1)));
        let x = makeXmlExpr(XmlExprOp::IS_XMLPARSE, None, List::new(), args, at1);
        Ok(Some(XmlExpr { xmloption: XmlOptionType(v3), ..x }.into()))
    }

    fn func_expr_common_subexpr_44(
        &mut self,
        v4: Option<Str>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLPI, v4, List::new(), List::new(), at1).into()))
    }

    fn func_expr_common_subexpr_45(
        &mut self,
        v4: Option<Str>,
        v6: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLPI, v4, List::new(), list_make1(v6), at1).into()))
    }

    fn func_expr_common_subexpr_46(
        &mut self,
        v3: Option<Node>,
        v5: Option<Node>,
        v6: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let args = vec![v3, v5, v6];
        Ok(Some(makeXmlExpr(XmlExprOp::IS_XMLROOT, None, List::new(), args, at1).into()))
    }

    fn func_expr_common_subexpr_47(
        &mut self,
        v3: i32,
        v4: Option<Node>,
        v6: Option<Box<TypeName>>,
        v7: bool,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = XmlSerialize {
            xmloption: XmlOptionType(v3),
            expr: v4,
            typeName: v6,
            indent: v7,
            location: at1,
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_48(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        // The `json_object()` function of before SQL/JSON, which is not in the standard.
        Ok(Some(call(SystemFuncName("json_object"), v3, at1).into()))
    }

    fn func_expr_common_subexpr_49(
        &mut self,
        v3: List,
        v4: bool,
        v5: bool,
        v6: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonObjectConstructor {
            exprs: v3,
            absent_on_null: v4,
            unique: v5,
            output: castNode(v6)?,
            location: at1,
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_50(
        &mut self,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonObjectConstructor {
            exprs: List::new(),
            absent_on_null: false,
            unique: false,
            output: castNode(v3)?,
            location: at1,
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_51(
        &mut self,
        v3: List,
        v4: bool,
        v5: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonArrayConstructor {
            exprs: v3,
            absent_on_null: v4,
            output: castNode(v5)?,
            location: at1,
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_52(
        &mut self,
        v3: Option<Node>,
        v4: Option<Node>,
        v5: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonArrayQueryConstructor {
            query: v3,
            format: castNode(v4)?,
            // PostgreSQL marks this value with `XXX`.
            absent_on_null: true,
            output: castNode(v5)?,
            location: at1,
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_53(
        &mut self,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonArrayConstructor {
            exprs: List::new(),
            absent_on_null: true,
            output: castNode(v3)?,
            location: at1,
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_54(
        &mut self,
        v3: Option<Node>,
        v4: bool,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonParseExpr { expr: castNode(v3)?, unique_keys: v4, output: None, location: at1 };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_55(
        &mut self,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(JsonScalarExpr { expr: v3, output: None, location: at1 }.into()))
    }

    fn func_expr_common_subexpr_56(
        &mut self,
        v3: Option<Node>,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonSerializeExpr { expr: castNode(v3)?, output: castNode(v4)?, location: at1 };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_57(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(
            MergeSupportFunc { msftype: TEXTOID, location: at1, ..MergeSupportFunc::default() }
                .into(),
        ))
    }

    fn func_expr_common_subexpr_58(
        &mut self,
        v3: Option<Node>,
        v5: Option<Node>,
        v6: List,
        v7: Option<Node>,
        v8: i32,
        v9: i32,
        v10: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let (on_empty, on_error) = behaviors(v10)?;
        let n = JsonFuncExpr {
            output: castNode(v7)?,
            wrapper: JsonWrapper(v8),
            quotes: JsonQuotes(v9),
            on_empty,
            on_error,
            ..query_function(JsonExprOp::JSON_QUERY_OP, v3, v5, v6, at1)?
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_59(
        &mut self,
        v3: Option<Node>,
        v5: Option<Node>,
        v6: List,
        v7: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonFuncExpr {
            output: None,
            on_error: castNode(v7)?,
            ..query_function(JsonExprOp::JSON_EXISTS_OP, v3, v5, v6, at1)?
        };
        Ok(Some(n.into()))
    }

    fn func_expr_common_subexpr_60(
        &mut self,
        v3: Option<Node>,
        v5: Option<Node>,
        v6: List,
        v7: Option<Node>,
        v8: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let (on_empty, on_error) = behaviors(v8)?;
        let n = JsonFuncExpr {
            output: castNode(v7)?,
            on_empty,
            on_error,
            ..query_function(JsonExprOp::JSON_VALUE_OP, v3, v5, v6, at1)?
        };
        Ok(Some(n.into()))
    }
}

impl rules::xml_root_version for Parser<'_> {
    fn xml_root_version_2(&mut self) -> Result<Option<Node>, Error> {
        Ok(Some(makeNullAConst(-1)))
    }
}

impl rules::opt_xml_root_standalone for Parser<'_> {
    fn opt_xml_root_standalone_1(&mut self) -> Result<Option<Node>, Error> {
        Ok(Some(makeIntConst(XmlStandaloneType::XML_STANDALONE_YES.0, -1)))
    }

    fn opt_xml_root_standalone_2(&mut self) -> Result<Option<Node>, Error> {
        Ok(Some(makeIntConst(XmlStandaloneType::XML_STANDALONE_NO.0, -1)))
    }

    fn opt_xml_root_standalone_3(&mut self) -> Result<Option<Node>, Error> {
        Ok(Some(makeIntConst(XmlStandaloneType::XML_STANDALONE_NO_VALUE.0, -1)))
    }

    fn opt_xml_root_standalone_4(&mut self) -> Result<Option<Node>, Error> {
        Ok(Some(makeIntConst(XmlStandaloneType::XML_STANDALONE_OMITTED.0, -1)))
    }
}

/// A `ResTarget` with the name `name`, no indirection and the value `val`.
pub(super) fn res_target(
    name: Option<Str>,
    val: Option<Node>,
    location: i32,
) -> Option<Box<ResTarget>> {
    Some(Box::new(ResTarget { name, indirection: List::new(), val, location }))
}

impl rules::labeled_expr for Parser<'_> {
    fn labeled_expr_1(
        &mut self,
        v1: Option<Node>,
        v3: Option<Str>,
        at1: i32,
    ) -> Result<Option<Box<ResTarget>>, Error> {
        Ok(res_target(v3, v1, at1))
    }

    fn labeled_expr_2(
        &mut self,
        v1: Option<Node>,
        at1: i32,
    ) -> Result<Option<Box<ResTarget>>, Error> {
        Ok(res_target(None, v1, at1))
    }
}

impl rules::xml_namespace_el for Parser<'_> {
    fn xml_namespace_el_1(
        &mut self,
        v1: Option<Node>,
        v3: Option<Str>,
        at1: i32,
    ) -> Result<Option<Box<ResTarget>>, Error> {
        Ok(res_target(v3, v1, at1))
    }

    fn xml_namespace_el_2(
        &mut self,
        v2: Option<Node>,
        at1: i32,
    ) -> Result<Option<Box<ResTarget>>, Error> {
        Ok(res_target(None, v2, at1))
    }
}

impl rules::window_definition for Parser<'_> {
    fn window_definition_1(
        &mut self,
        v1: Option<Str>,
        v3: Option<Box<WindowDef>>,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(change(v3, |n| n.name = v1))
    }
}

impl rules::over_clause for Parser<'_> {
    fn over_clause_2(
        &mut self,
        v2: Option<Str>,
        at2: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        let n = WindowDef {
            name: v2,
            frameOptions: FRAMEOPTION_DEFAULTS,
            location: at2,
            ..WindowDef::default()
        };
        Ok(Some(Box::new(n)))
    }
}

impl rules::window_specification for Parser<'_> {
    fn window_specification_1(
        &mut self,
        v2: Option<Str>,
        v3: List,
        v4: List,
        v5: Option<Box<WindowDef>>,
        at1: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        // Takes the frame fields of `opt_frame_clause`.
        let frame = v5.map(|f| *f).unwrap_or_default();
        let n = WindowDef {
            name: None,
            refname: v2,
            partitionClause: v3,
            orderClause: v4,
            frameOptions: frame.frameOptions,
            startOffset: frame.startOffset,
            endOffset: frame.endOffset,
            location: at1,
        };
        Ok(Some(Box::new(n)))
    }
}

/// `opt_frame_clause` of the mode `mode`: the frame of `frame_extent` with the mode and the
/// exclusion.
fn frame(n: Option<Box<WindowDef>>, mode: i32, exclusion: i32) -> Option<Box<WindowDef>> {
    change(n, |n| n.frameOptions |= FRAMEOPTION_NONDEFAULT | mode | exclusion)
}

impl rules::opt_frame_clause for Parser<'_> {
    fn opt_frame_clause_1(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v3: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame(v2, FRAMEOPTION_RANGE, v3))
    }

    fn opt_frame_clause_2(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v3: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame(v2, FRAMEOPTION_ROWS, v3))
    }

    fn opt_frame_clause_3(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v3: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame(v2, FRAMEOPTION_GROUPS, v3))
    }

    fn opt_frame_clause_4(&mut self) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame_bound(FRAMEOPTION_DEFAULTS, None))
    }
}

impl rules::frame_extent for Parser<'_> {
    fn frame_extent_1(
        &mut self,
        v1: Option<Box<WindowDef>>,
        at1: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        let mut v1 = v1;
        if let Some(n) = v1.as_mut() {
            // Reject the cases that are not valid.
            if n.frameOptions & FRAMEOPTION_START_UNBOUNDED_FOLLOWING != 0 {
                let message = "frame start cannot be UNBOUNDED FOLLOWING";
                return Err(self.error(ERRCODE_WINDOWING_ERROR, message, at1));
            }
            if n.frameOptions & FRAMEOPTION_START_OFFSET_FOLLOWING != 0 {
                let message = "frame starting from following row cannot end with current row";
                return Err(self.error(ERRCODE_WINDOWING_ERROR, message, at1));
            }
            n.frameOptions |= FRAMEOPTION_END_CURRENT_ROW;
        }
        Ok(v1)
    }

    fn frame_extent_2(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v4: Option<Box<WindowDef>>,
        at2: i32,
        at4: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        let mut n1 = v2.unwrap_or_default();
        let n2 = v4.unwrap_or_default();
        // The shift makes the `START_` options of the end bound into `END_` options.
        let frameOptions = n1.frameOptions | n2.frameOptions << 1 | FRAMEOPTION_BETWEEN;
        // Reject the cases that are not valid.
        let error = |message, location| Err(self.error(ERRCODE_WINDOWING_ERROR, message, location));
        if frameOptions & FRAMEOPTION_START_UNBOUNDED_FOLLOWING != 0 {
            return error("frame start cannot be UNBOUNDED FOLLOWING", at2);
        }
        if frameOptions & FRAMEOPTION_END_UNBOUNDED_PRECEDING != 0 {
            return error("frame end cannot be UNBOUNDED PRECEDING", at4);
        }
        if frameOptions & FRAMEOPTION_START_CURRENT_ROW != 0
            && frameOptions & FRAMEOPTION_END_OFFSET_PRECEDING != 0
        {
            return error("frame starting from current row cannot have preceding rows", at4);
        }
        if frameOptions & FRAMEOPTION_START_OFFSET_FOLLOWING != 0
            && frameOptions & (FRAMEOPTION_END_OFFSET_PRECEDING | FRAMEOPTION_END_CURRENT_ROW) != 0
        {
            return error("frame starting from following row cannot have preceding rows", at4);
        }
        n1.frameOptions = frameOptions;
        n1.endOffset = n2.startOffset;
        Ok(Some(n1))
    }
}

/// A `WindowDef` with the frame options `frameOptions` and the start offset `startOffset`, as
/// `frame_bound` makes it.
fn frame_bound(frameOptions: i32, startOffset: Option<Node>) -> Option<Box<WindowDef>> {
    Some(Box::new(WindowDef { frameOptions, startOffset, endOffset: None, ..WindowDef::default() }))
}

impl rules::frame_bound for Parser<'_> {
    fn frame_bound_1(&mut self) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame_bound(FRAMEOPTION_START_UNBOUNDED_PRECEDING, None))
    }

    fn frame_bound_2(&mut self) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame_bound(FRAMEOPTION_START_UNBOUNDED_FOLLOWING, None))
    }

    fn frame_bound_3(&mut self) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame_bound(FRAMEOPTION_START_CURRENT_ROW, None))
    }

    fn frame_bound_4(&mut self, v1: Option<Node>) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame_bound(FRAMEOPTION_START_OFFSET_PRECEDING, v1))
    }

    fn frame_bound_5(&mut self, v1: Option<Node>) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame_bound(FRAMEOPTION_START_OFFSET_FOLLOWING, v1))
    }
}

impl rules::func_arg_expr for Parser<'_> {
    fn func_arg_expr_2(
        &mut self,
        v1: Option<Str>,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // The analysis sets the argument number.
        Ok(Some(NamedArgExpr { name: v1, arg: v3, argnumber: -1, location: at1 }.into()))
    }

    fn func_arg_expr_3(
        &mut self,
        v1: Option<Str>,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(NamedArgExpr { name: v1, arg: v3, argnumber: -1, location: at1 }.into()))
    }
}

impl rules::array_expr for Parser<'_> {
    fn array_expr_1(&mut self, v2: List, at1: i32, at3: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeAArrayExpr(v2, at1, at3)))
    }

    fn array_expr_2(&mut self, v2: List, at1: i32, at3: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeAArrayExpr(v2, at1, at3)))
    }

    fn array_expr_3(&mut self, at1: i32, at2: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeAArrayExpr(List::new(), at1, at2)))
    }
}

impl rules::extract_list for Parser<'_> {
    fn extract_list_1(
        &mut self,
        v1: Option<Str>,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<List, Error> {
        Ok(list_make2(Some(makeStringConst(v1, at1)), v3))
    }
}

impl rules::overlay_list for Parser<'_> {
    fn overlay_list_1(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v5: Option<Node>,
        v7: Option<Node>,
    ) -> Result<List, Error> {
        // `overlay(A PLACING B FROM C FOR D)` is `overlay(A, B, C, D)`.
        Ok(vec![v1, v3, v5, v7])
    }

    fn overlay_list_2(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v5: Option<Node>,
    ) -> Result<List, Error> {
        // `overlay(A PLACING B FROM C)` is `overlay(A, B, C)`.
        Ok(vec![v1, v3, v5])
    }
}

impl rules::substr_list for Parser<'_> {
    fn substr_list_1(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v5: Option<Node>,
    ) -> Result<List, Error> {
        Ok(vec![v1, v3, v5])
    }

    fn substr_list_2(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v5: Option<Node>,
    ) -> Result<List, Error> {
        // This order is not in the SQL standard, but it is accepted.
        Ok(vec![v1, v5, v3])
    }

    fn substr_list_4(&mut self, v1: Option<Node>, v3: Option<Node>) -> Result<List, Error> {
        // This form is not in the SQL standard. The `FOR` value is never text here, so the cast
        // to `int4` makes sure that the analysis picks `substring(text, int4)` and not
        // `substring(text, text)`.
        let length = makeTypeCast(v3, Some(Box::new(SystemTypeName("int4"))), -1);
        Ok(vec![v1, Some(makeIntConst(1, -1)), Some(length)])
    }

    fn substr_list_5(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v5: Option<Node>,
    ) -> Result<List, Error> {
        Ok(vec![v1, v3, v5])
    }
}

impl rules::case_expr for Parser<'_> {
    fn case_expr_1(
        &mut self,
        v2: Option<Node>,
        v3: List,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // The analysis sets the type.
        let c = CaseExpr { arg: v2, args: v3, defresult: v4, location: at1, ..CaseExpr::default() };
        Ok(Some(c.into()))
    }
}

impl rules::when_clause for Parser<'_> {
    fn when_clause_1(
        &mut self,
        v2: Option<Node>,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(CaseWhen { expr: v2, result: v4, location: at1 }.into()))
    }
}

impl rules::columnref for Parser<'_> {
    fn columnref_1(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeColumnRef(v1, List::new(), at1, self)?))
    }

    fn columnref_2(&mut self, v1: Option<Str>, v2: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeColumnRef(v1, v2, at1, self)?))
    }
}

impl rules::indirection_el for Parser<'_> {
    fn indirection_el_1(&mut self, v2: Option<Str>) -> Result<Option<Node>, Error> {
        Ok(Some(makeString(v2)))
    }

    fn indirection_el_2(&mut self) -> Result<Option<Node>, Error> {
        Ok(Some(A_Star::default().into()))
    }

    fn indirection_el_3(&mut self, v2: Option<Node>) -> Result<Option<Node>, Error> {
        Ok(Some(A_Indices { is_slice: false, lidx: None, uidx: v2 }.into()))
    }

    fn indirection_el_4(
        &mut self,
        v2: Option<Node>,
        v4: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(A_Indices { is_slice: true, lidx: v2, uidx: v4 }.into()))
    }
}

impl rules::target_el for Parser<'_> {
    fn target_el_1(
        &mut self,
        v1: Option<Node>,
        v3: Option<Str>,
        at1: i32,
    ) -> Result<Option<Box<ResTarget>>, Error> {
        Ok(res_target(v3, v1, at1))
    }

    fn target_el_2(
        &mut self,
        v1: Option<Node>,
        v2: Option<Str>,
        at1: i32,
    ) -> Result<Option<Box<ResTarget>>, Error> {
        Ok(res_target(v2, v1, at1))
    }

    fn target_el_3(&mut self, v1: Option<Node>, at1: i32) -> Result<Option<Box<ResTarget>>, Error> {
        Ok(res_target(None, v1, at1))
    }

    fn target_el_4(&mut self, at1: i32) -> Result<Option<Box<ResTarget>>, Error> {
        let n = ColumnRef { fields: list_make1(Some(A_Star::default().into())), location: at1 };
        Ok(res_target(None, Some(n.into()), at1))
    }
}
