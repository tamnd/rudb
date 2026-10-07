//! The actions of SQL/JSON: the `IS JSON` predicate, the constructors, the query functions, the
//! aggregates and `JSON_TABLE`, with their clauses. The alternatives of `a_expr`, `func_expr` and
//! `func_expr_common_subexpr` for SQL/JSON are in `expr.rs` with the other alternatives of those
//! rules, and use the functions here.

use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

/// `makeJsonFormat(JS_FORMAT_DEFAULT, JS_ENC_DEFAULT, -1)`.
pub(super) fn default_format() -> JsonFormat {
    makeJsonFormat(JsonFormatType::JS_FORMAT_DEFAULT, JsonEncoding::JS_ENC_DEFAULT, -1)
}

/// `linitial` and `lsecond` of `json_behavior_clause_opt`: the behavior `ON EMPTY` and the
/// behavior `ON ERROR`.
type Behaviors = (Option<Box<JsonBehavior>>, Option<Box<JsonBehavior>>);

pub(super) fn behaviors(list: List) -> Result<Behaviors, Error> {
    let [on_empty, on_error] = elements(list);
    Ok((castNode(on_empty)?, castNode(on_error)?))
}

/// A `JsonFuncExpr` with the fields that all the query functions set.
pub(super) fn query_function(
    op: JsonExprOp,
    context_item: Option<Node>,
    pathspec: Option<Node>,
    passing: List,
    location: i32,
) -> Result<JsonFuncExpr, Error> {
    Ok(JsonFuncExpr {
        op,
        context_item: castNode(context_item)?,
        pathspec,
        passing,
        location,
        ..JsonFuncExpr::default()
    })
}

/// `makeJsonIsPredicate($1, format, type, unique, InvalidOid, @1)` with the default format, for
/// `a_expr IS [NOT] JSON`.
pub(super) fn is_json(
    expr: Option<Node>,
    item_type: i32,
    unique_keys: bool,
    location: i32,
) -> Node {
    let item_type = JsonValueType(item_type);
    makeJsonIsPredicate(expr, default_format(), item_type, unique_keys, InvalidOid, location)
}

/// A `JsonAggConstructor` with an output, an order and a location.
fn agg_constructor(
    output: Option<Node>,
    agg_order: List,
    location: i32,
) -> Result<Option<Box<JsonAggConstructor>>, Error> {
    Ok(Some(Box::new(JsonAggConstructor {
        output: castNode(output)?,
        agg_order,
        location,
        ..JsonAggConstructor::default()
    })))
}

impl rules::json_table for Parser<'_> {
    fn json_table_1(
        &mut self,
        v3: Option<Node>,
        v5: Option<Node>,
        v6: Option<Str>,
        v7: List,
        v10: List,
        v12: Option<Node>,
        at1: i32,
        at5: i32,
        at6: i32,
    ) -> Result<Option<Node>, Error> {
        let pathstring = match v5.as_ref().and_then(A_Const::peek) {
            Some(A_Const { val: Some(Node::String(s)), .. }) => s.clone(),
            _ => {
                let message =
                    "only string constants are supported in JSON_TABLE path specification";
                return Err(self.error(ERRCODE_FEATURE_NOT_SUPPORTED, message, at5));
            }
        };
        let n = JsonTable {
            context_item: castNode(v3)?,
            pathspec: Some(Box::new(makeJsonTablePathSpec(Some(pathstring), v6, at5, at6))),
            passing: v7,
            columns: v10,
            on_error: castNode(v12)?,
            location: at1,
            ..JsonTable::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::json_table_column_definition for Parser<'_> {
    fn json_table_column_definition_1(
        &mut self,
        v1: Option<Str>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonTableColumn {
            coltype: JsonTableColumnType::JTC_FOR_ORDINALITY,
            name: v1,
            location: at1,
            ..JsonTableColumn::default()
        };
        Ok(Some(n.into()))
    }

    fn json_table_column_definition_2(
        &mut self,
        v1: Option<Str>,
        v2: Option<Box<TypeName>>,
        v3: Option<Node>,
        v4: i32,
        v5: i32,
        v6: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let (on_empty, on_error) = behaviors(v6)?;
        let n = JsonTableColumn {
            coltype: JsonTableColumnType::JTC_REGULAR,
            name: v1,
            typeName: v2,
            format: Some(Box::new(default_format())),
            pathspec: castNode(v3)?,
            wrapper: JsonWrapper(v4),
            quotes: JsonQuotes(v5),
            on_empty,
            on_error,
            location: at1,
            ..JsonTableColumn::default()
        };
        Ok(Some(n.into()))
    }

    fn json_table_column_definition_3(
        &mut self,
        v1: Option<Str>,
        v2: Option<Box<TypeName>>,
        v3: Option<Node>,
        v4: Option<Node>,
        v5: i32,
        v6: i32,
        v7: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let (on_empty, on_error) = behaviors(v7)?;
        let n = JsonTableColumn {
            coltype: JsonTableColumnType::JTC_FORMATTED,
            name: v1,
            typeName: v2,
            format: castNode(v3)?,
            pathspec: castNode(v4)?,
            wrapper: JsonWrapper(v5),
            quotes: JsonQuotes(v6),
            on_empty,
            on_error,
            location: at1,
            ..JsonTableColumn::default()
        };
        Ok(Some(n.into()))
    }

    fn json_table_column_definition_4(
        &mut self,
        v1: Option<Str>,
        v2: Option<Box<TypeName>>,
        v4: Option<Node>,
        v5: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonTableColumn {
            coltype: JsonTableColumnType::JTC_EXISTS,
            name: v1,
            typeName: v2,
            format: Some(Box::new(default_format())),
            wrapper: JsonWrapper::JSW_NONE,
            quotes: JsonQuotes::JS_QUOTES_UNSPEC,
            pathspec: castNode(v4)?,
            on_empty: None,
            on_error: castNode(v5)?,
            location: at1,
            ..JsonTableColumn::default()
        };
        Ok(Some(n.into()))
    }

    fn json_table_column_definition_5(
        &mut self,
        v3: Option<Str>,
        v6: List,
        at1: i32,
        at3: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonTableColumn {
            coltype: JsonTableColumnType::JTC_NESTED,
            pathspec: Some(Box::new(makeJsonTablePathSpec(v3, None, at3, -1))),
            columns: v6,
            location: at1,
            ..JsonTableColumn::default()
        };
        Ok(Some(n.into()))
    }

    fn json_table_column_definition_6(
        &mut self,
        v3: Option<Str>,
        v5: Option<Str>,
        v8: List,
        at1: i32,
        at3: i32,
        at5: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonTableColumn {
            coltype: JsonTableColumnType::JTC_NESTED,
            pathspec: Some(Box::new(makeJsonTablePathSpec(v3, v5, at3, at5))),
            columns: v8,
            location: at1,
            ..JsonTableColumn::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::json_table_column_path_clause_opt for Parser<'_> {
    fn json_table_column_path_clause_opt_1(
        &mut self,
        v2: Option<Str>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeJsonTablePathSpec(v2, None, at2, -1).into()))
    }
}

impl rules::json_argument for Parser<'_> {
    fn json_argument_1(
        &mut self,
        v1: Option<Node>,
        v3: Option<Str>,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(JsonArgument { val: castNode(v1)?, name: v3 }.into()))
    }
}

impl rules::json_behavior for Parser<'_> {
    fn json_behavior_1(&mut self, v2: Option<Node>, at1: i32) -> Result<Option<Node>, Error> {
        let btype = JsonBehaviorType::JSON_BEHAVIOR_DEFAULT;
        Ok(Some(makeJsonBehavior(btype, v2, at1).into()))
    }

    fn json_behavior_2(&mut self, v1: i32, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeJsonBehavior(JsonBehaviorType(v1), None, at1).into()))
    }
}

impl rules::json_value_expr for Parser<'_> {
    fn json_value_expr_1(
        &mut self,
        v1: Option<Node>,
        v2: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        // `formatted_expr` is set in the parse analysis.
        Ok(Some(makeJsonValueExpr(v1, None, castNode(v2)?).into()))
    }
}

impl rules::json_format_clause for Parser<'_> {
    fn json_format_clause_1(
        &mut self,
        v4: Option<Str>,
        at1: i32,
        at4: i32,
    ) -> Result<Option<Node>, Error> {
        // `pg_strcasecmp`, which folds the case of the ASCII letters only.
        let name = v4.as_deref().unwrap_or_default();
        let encoding = if name.eq_ignore_ascii_case("utf8") {
            JsonEncoding::JS_ENC_UTF8
        } else if name.eq_ignore_ascii_case("utf16") {
            JsonEncoding::JS_ENC_UTF16
        } else if name.eq_ignore_ascii_case("utf32") {
            JsonEncoding::JS_ENC_UTF32
        } else {
            let message = format!("unrecognized JSON encoding: {name}");
            return Err(self.error(ERRCODE_INVALID_PARAMETER_VALUE, &message, at4));
        };
        Ok(Some(makeJsonFormat(JsonFormatType::JS_FORMAT_JSON, encoding, at1).into()))
    }

    fn json_format_clause_2(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        let format_type = JsonFormatType::JS_FORMAT_JSON;
        Ok(Some(makeJsonFormat(format_type, JsonEncoding::JS_ENC_DEFAULT, at1).into()))
    }
}

impl rules::json_format_clause_opt for Parser<'_> {
    fn json_format_clause_opt_2(&mut self) -> Result<Option<Node>, Error> {
        Ok(Some(default_format().into()))
    }
}

impl rules::json_returning_clause_opt for Parser<'_> {
    fn json_returning_clause_opt_1(
        &mut self,
        v2: Option<Box<TypeName>>,
        v3: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        let returning = JsonReturning { format: castNode(v3)?, ..JsonReturning::default() };
        Ok(Some(JsonOutput { typeName: v2, returning: Some(Box::new(returning)) }.into()))
    }
}

impl rules::json_name_and_value for Parser<'_> {
    fn json_name_and_value_1(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeJsonKeyValue(v1, v3)?))
    }

    fn json_name_and_value_2(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeJsonKeyValue(v1, v3)?))
    }
}

impl rules::json_aggregate_func for Parser<'_> {
    fn json_aggregate_func_1(
        &mut self,
        v3: Option<Node>,
        v4: bool,
        v5: bool,
        v6: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonObjectAgg {
            arg: castNode(v3)?,
            absent_on_null: v4,
            unique: v5,
            constructor: agg_constructor(v6, List::new(), at1)?,
        };
        Ok(Some(n.into()))
    }

    fn json_aggregate_func_2(
        &mut self,
        v3: Option<Node>,
        v4: List,
        v5: bool,
        v6: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = JsonArrayAgg {
            arg: castNode(v3)?,
            absent_on_null: v5,
            constructor: agg_constructor(v6, v4, at1)?,
        };
        Ok(Some(n.into()))
    }
}
