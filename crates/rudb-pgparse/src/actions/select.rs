//! The actions of `SELECT`: the select clauses, `WITH`, `INTO`, `ORDER BY`, `LIMIT`, `GROUP BY`,
//! the locking clauses, `VALUES`, and the `FROM` list with its table references and joins.

use super::*;
use crate::error::Error;
use crate::generated::glue::{GroupClause, SelectLimit, rules};
use crate::nodes::*;

impl rules::select_no_parens for Parser<'_> {
    fn select_no_parens_2(&mut self, v1: Option<Node>, v2: List) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        insertSelectOptions(castMut(&mut v1)?, v2, List::new(), None, None, self)?;
        Ok(v1)
    }

    fn select_no_parens_3(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: List,
        v4: Option<Box<SelectLimit>>,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        insertSelectOptions(castMut(&mut v1)?, v2, v3, v4, None, self)?;
        Ok(v1)
    }

    fn select_no_parens_4(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: Option<Box<SelectLimit>>,
        v4: List,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        insertSelectOptions(castMut(&mut v1)?, v2, v4, v3, None, self)?;
        Ok(v1)
    }

    fn select_no_parens_5(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, List::new(), List::new(), None, v1, self)?;
        Ok(v2)
    }

    fn select_no_parens_6(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
        v3: List,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, v3, List::new(), None, v1, self)?;
        Ok(v2)
    }

    fn select_no_parens_7(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
        v3: List,
        v4: List,
        v5: Option<Box<SelectLimit>>,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, v3, v4, v5, v1, self)?;
        Ok(v2)
    }

    fn select_no_parens_8(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
        v3: List,
        v4: Option<Box<SelectLimit>>,
        v5: List,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, v3, v5, v4, v1, self)?;
        Ok(v2)
    }
}

/// The `SelectStmt` of the two forms of `SELECT ... FROM ... WHERE ...`.
#[allow(clippy::too_many_arguments)]
pub(super) fn select(
    distinctClause: List,
    targetList: List,
    intoClause: Option<Box<IntoClause>>,
    fromClause: List,
    whereClause: Option<Node>,
    groupClause: Option<Box<GroupClause>>,
    havingClause: Option<Node>,
    windowClause: List,
) -> Option<Node> {
    let group = groupClause.map(|g| *g).unwrap_or_default();
    let n = SelectStmt {
        distinctClause,
        targetList,
        intoClause,
        fromClause,
        whereClause,
        groupClause: group.list,
        groupDistinct: group.distinct,
        havingClause,
        windowClause,
        ..SelectStmt::default()
    };
    Some(n.into())
}

impl rules::simple_select for Parser<'_> {
    fn simple_select_1(
        &mut self,
        v3: List,
        v4: Option<Box<IntoClause>>,
        v5: List,
        v6: Option<Node>,
        v7: Option<Box<GroupClause>>,
        v8: Option<Node>,
        v9: List,
    ) -> Result<Option<Node>, Error> {
        Ok(select(List::new(), v3, v4, v5, v6, v7, v8, v9))
    }

    fn simple_select_2(
        &mut self,
        v2: List,
        v3: List,
        v4: Option<Box<IntoClause>>,
        v5: List,
        v6: Option<Node>,
        v7: Option<Box<GroupClause>>,
        v8: Option<Node>,
        v9: List,
    ) -> Result<Option<Node>, Error> {
        Ok(select(v2, v3, v4, v5, v6, v7, v8, v9))
    }

    fn simple_select_4(&mut self, v2: Option<Box<RangeVar>>) -> Result<Option<Node>, Error> {
        // The same as `SELECT * FROM relation_expr`.
        let cr = ColumnRef { fields: list_make1(Some(A_Star::default().into())), location: -1 };
        let rt =
            ResTarget { name: None, indirection: List::new(), val: Some(cr.into()), location: -1 };
        let n = SelectStmt {
            targetList: list_make1(Some(rt.into())),
            fromClause: list_make1(v2.map(NodeType::into_node)),
            ..SelectStmt::default()
        };
        Ok(Some(n.into()))
    }

    fn simple_select_5(
        &mut self,
        v1: Option<Node>,
        v3: SetQuantifier,
        v4: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSetOp(
            SetOperation::SETOP_UNION,
            v3 == SetQuantifier::SET_QUANTIFIER_ALL,
            v1,
            v4,
        )))
    }

    fn simple_select_6(
        &mut self,
        v1: Option<Node>,
        v3: SetQuantifier,
        v4: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSetOp(
            SetOperation::SETOP_INTERSECT,
            v3 == SetQuantifier::SET_QUANTIFIER_ALL,
            v1,
            v4,
        )))
    }

    fn simple_select_7(
        &mut self,
        v1: Option<Node>,
        v3: SetQuantifier,
        v4: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSetOp(
            SetOperation::SETOP_EXCEPT,
            v3 == SetQuantifier::SET_QUANTIFIER_ALL,
            v1,
            v4,
        )))
    }
}

/// The `WithClause` of the three forms of `WITH`.
fn with(ctes: List, recursive: bool, location: i32) -> Option<Box<WithClause>> {
    Some(Box::new(WithClause { ctes, recursive, location }))
}

impl rules::with_clause for Parser<'_> {
    fn with_clause_1(&mut self, v2: List, at1: i32) -> Result<Option<Box<WithClause>>, Error> {
        Ok(with(v2, false, at1))
    }

    fn with_clause_2(&mut self, v2: List, at1: i32) -> Result<Option<Box<WithClause>>, Error> {
        Ok(with(v2, false, at1))
    }

    fn with_clause_3(&mut self, v3: List, at1: i32) -> Result<Option<Box<WithClause>>, Error> {
        Ok(with(v3, true, at1))
    }
}

impl rules::common_table_expr for Parser<'_> {
    #[allow(clippy::too_many_arguments)]
    fn common_table_expr_1(
        &mut self,
        v1: Option<Str>,
        v2: List,
        v4: i32,
        v6: Option<Node>,
        v8: Option<Node>,
        v9: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = CommonTableExpr {
            ctename: v1,
            aliascolnames: v2,
            ctematerialized: CTEMaterialize(v4),
            ctequery: v6,
            search_clause: castNode(v8)?,
            cycle_clause: castNode(v9)?,
            location: at1,
            ..CommonTableExpr::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::opt_search_clause for Parser<'_> {
    fn opt_search_clause_1(
        &mut self,
        v5: List,
        v7: Option<Str>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = CTESearchClause {
            search_col_list: v5,
            search_breadth_first: false,
            search_seq_column: v7,
            location: at1,
        };
        Ok(Some(n.into()))
    }

    fn opt_search_clause_2(
        &mut self,
        v5: List,
        v7: Option<Str>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = CTESearchClause {
            search_col_list: v5,
            search_breadth_first: true,
            search_seq_column: v7,
            location: at1,
        };
        Ok(Some(n.into()))
    }
}

impl rules::opt_cycle_clause for Parser<'_> {
    fn opt_cycle_clause_1(
        &mut self,
        v2: List,
        v4: Option<Str>,
        v6: Option<Node>,
        v8: Option<Node>,
        v10: Option<Str>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = CTECycleClause {
            cycle_col_list: v2,
            cycle_mark_column: v4,
            cycle_mark_value: v6,
            cycle_mark_default: v8,
            cycle_path_column: v10,
            location: at1,
            ..CTECycleClause::default()
        };
        Ok(Some(n.into()))
    }

    fn opt_cycle_clause_2(
        &mut self,
        v2: List,
        v4: Option<Str>,
        v6: Option<Str>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = CTECycleClause {
            cycle_col_list: v2,
            cycle_mark_column: v4,
            cycle_mark_value: Some(makeBoolAConst(true, -1)),
            cycle_mark_default: Some(makeBoolAConst(false, -1)),
            cycle_path_column: v6,
            location: at1,
            ..CTECycleClause::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::into_clause for Parser<'_> {
    fn into_clause_1(
        &mut self,
        v2: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<IntoClause>>, Error> {
        let n = IntoClause {
            rel: v2,
            onCommit: OnCommitAction::ONCOMMIT_NOOP,
            ..IntoClause::default()
        };
        Ok(Some(Box::new(n)))
    }
}

/// Sets the persistence of the relation of `OptTempTableName`.
fn persistence(rel: Option<Box<RangeVar>>, relpersistence: u8) -> Option<Box<RangeVar>> {
    let mut rel = rel;
    if let Some(r) = rel.as_mut() {
        r.relpersistence = relpersistence;
    }
    rel
}

/// The warning of `GLOBAL TEMPORARY` and `GLOBAL TEMP`.
const GLOBAL_DEPRECATED: &str = "GLOBAL is deprecated in temporary table creation";

impl rules::OptTempTableName for Parser<'_> {
    fn OptTempTableName_1(
        &mut self,
        v3: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(persistence(v3, RELPERSISTENCE_TEMP))
    }

    fn OptTempTableName_2(
        &mut self,
        v3: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(persistence(v3, RELPERSISTENCE_TEMP))
    }

    fn OptTempTableName_3(
        &mut self,
        v4: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(persistence(v4, RELPERSISTENCE_TEMP))
    }

    fn OptTempTableName_4(
        &mut self,
        v4: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(persistence(v4, RELPERSISTENCE_TEMP))
    }

    fn OptTempTableName_5(
        &mut self,
        v4: Option<Box<RangeVar>>,
        at1: i32,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        self.warning(GLOBAL_DEPRECATED, at1);
        Ok(persistence(v4, RELPERSISTENCE_TEMP))
    }

    fn OptTempTableName_6(
        &mut self,
        v4: Option<Box<RangeVar>>,
        at1: i32,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        self.warning(GLOBAL_DEPRECATED, at1);
        Ok(persistence(v4, RELPERSISTENCE_TEMP))
    }

    fn OptTempTableName_7(
        &mut self,
        v3: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(persistence(v3, RELPERSISTENCE_UNLOGGED))
    }

    fn OptTempTableName_8(
        &mut self,
        v2: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(persistence(v2, RELPERSISTENCE_PERMANENT))
    }

    fn OptTempTableName_9(
        &mut self,
        v1: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(persistence(v1, RELPERSISTENCE_PERMANENT))
    }
}

impl rules::sortby for Parser<'_> {
    fn sortby_1(
        &mut self,
        v1: Option<Node>,
        v3: List,
        v4: i32,
        at3: i32,
    ) -> Result<Option<Box<SortBy>>, Error> {
        let n = SortBy {
            node: v1,
            sortby_dir: SortByDir::SORTBY_USING,
            sortby_nulls: SortByNulls(v4),
            useOp: v3,
            location: at3,
        };
        Ok(Some(Box::new(n)))
    }

    fn sortby_2(
        &mut self,
        v1: Option<Node>,
        v2: i32,
        v3: i32,
    ) -> Result<Option<Box<SortBy>>, Error> {
        let n = SortBy {
            node: v1,
            sortby_dir: SortByDir(v2),
            sortby_nulls: SortByNulls(v3),
            useOp: List::new(),
            // No operator.
            location: -1,
        };
        Ok(Some(Box::new(n)))
    }
}

impl rules::select_limit for Parser<'_> {
    fn select_limit_1(
        &mut self,
        v1: Option<Box<SelectLimit>>,
        v2: Option<Node>,
        at2: i32,
    ) -> Result<Option<Box<SelectLimit>>, Error> {
        let mut v1 = v1;
        if let Some(n) = v1.as_mut() {
            n.limitOffset = v2;
            n.offsetLoc = at2;
        }
        Ok(v1)
    }

    fn select_limit_2(
        &mut self,
        v1: Option<Node>,
        v2: Option<Box<SelectLimit>>,
        at1: i32,
    ) -> Result<Option<Box<SelectLimit>>, Error> {
        let mut v2 = v2;
        if let Some(n) = v2.as_mut() {
            n.limitOffset = v1;
            n.offsetLoc = at1;
        }
        Ok(v2)
    }

    fn select_limit_4(
        &mut self,
        v1: Option<Node>,
        at1: i32,
    ) -> Result<Option<Box<SelectLimit>>, Error> {
        Ok(limit(v1, None, LimitOption::LIMIT_OPTION_COUNT, at1, -1, -1))
    }
}

/// A `SelectLimit`.
fn limit(
    limitOffset: Option<Node>,
    limitCount: Option<Node>,
    limitOption: LimitOption,
    offsetLoc: i32,
    countLoc: i32,
    optionLoc: i32,
) -> Option<Box<SelectLimit>> {
    Some(Box::new(SelectLimit {
        limitOffset,
        limitCount,
        limitOption,
        offsetLoc,
        countLoc,
        optionLoc,
    }))
}

impl rules::limit_clause for Parser<'_> {
    fn limit_clause_1(
        &mut self,
        v2: Option<Node>,
        at1: i32,
    ) -> Result<Option<Box<SelectLimit>>, Error> {
        Ok(limit(None, v2, LimitOption::LIMIT_OPTION_COUNT, -1, at1, -1))
    }

    fn limit_clause_2(&mut self, at1: i32) -> Result<Option<Box<SelectLimit>>, Error> {
        // Disabled because it was too confusing, bjm 2002-02-18.
        let mut error = self.error(ERRCODE_SYNTAX_ERROR, "LIMIT #,# syntax is not supported", at1);
        error.hint = Some("Use separate LIMIT and OFFSET clauses.");
        Err(error)
    }

    fn limit_clause_3(
        &mut self,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Box<SelectLimit>>, Error> {
        Ok(limit(None, v3, LimitOption::LIMIT_OPTION_COUNT, -1, at1, -1))
    }

    fn limit_clause_4(
        &mut self,
        v3: Option<Node>,
        at1: i32,
        at5: i32,
    ) -> Result<Option<Box<SelectLimit>>, Error> {
        Ok(limit(None, v3, LimitOption::LIMIT_OPTION_WITH_TIES, -1, at1, at5))
    }

    fn limit_clause_5(&mut self, at1: i32) -> Result<Option<Box<SelectLimit>>, Error> {
        Ok(limit(None, Some(makeIntConst(1, -1)), LimitOption::LIMIT_OPTION_COUNT, -1, at1, -1))
    }

    fn limit_clause_6(&mut self, at1: i32, at4: i32) -> Result<Option<Box<SelectLimit>>, Error> {
        Ok(limit(
            None,
            Some(makeIntConst(1, -1)),
            LimitOption::LIMIT_OPTION_WITH_TIES,
            -1,
            at1,
            at4,
        ))
    }
}

impl rules::select_limit_value for Parser<'_> {
    fn select_limit_value_2(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        // `LIMIT ALL` is a `NULL` constant.
        Ok(Some(makeNullAConst(at1)))
    }
}

impl rules::select_fetch_first_value for Parser<'_> {
    fn select_fetch_first_value_2(
        &mut self,
        v2: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(makeSimpleA_Expr(A_Expr_Kind::AEXPR_OP, "+", None, v2, at1).into()))
    }

    fn select_fetch_first_value_3(
        &mut self,
        v2: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(doNegate(v2, at1)))
    }
}

impl rules::I_or_F_const for Parser<'_> {
    fn I_or_F_const_1(&mut self, v1: i32, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeIntConst(v1, at1)))
    }

    fn I_or_F_const_2(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeFloatConst(v1, at1)))
    }
}

impl rules::group_clause for Parser<'_> {
    fn group_clause_1(
        &mut self,
        v3: SetQuantifier,
        v4: List,
    ) -> Result<Option<Box<GroupClause>>, Error> {
        let n = GroupClause { distinct: v3 == SetQuantifier::SET_QUANTIFIER_DISTINCT, list: v4 };
        Ok(Some(Box::new(n)))
    }

    fn group_clause_2(&mut self) -> Result<Option<Box<GroupClause>>, Error> {
        Ok(Some(Box::new(GroupClause { distinct: false, list: List::new() })))
    }
}

impl rules::empty_grouping_set for Parser<'_> {
    fn empty_grouping_set_1(&mut self, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeGroupingSet(GroupingSetKind::GROUPING_SET_EMPTY, List::new(), at1).into()))
    }
}

impl rules::rollup_clause for Parser<'_> {
    fn rollup_clause_1(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeGroupingSet(GroupingSetKind::GROUPING_SET_ROLLUP, v3, at1).into()))
    }
}

impl rules::cube_clause for Parser<'_> {
    fn cube_clause_1(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeGroupingSet(GroupingSetKind::GROUPING_SET_CUBE, v3, at1).into()))
    }
}

impl rules::grouping_sets_clause for Parser<'_> {
    fn grouping_sets_clause_1(&mut self, v4: List, at1: i32) -> Result<Option<Node>, Error> {
        Ok(Some(makeGroupingSet(GroupingSetKind::GROUPING_SET_SETS, v4, at1).into()))
    }
}

impl rules::for_locking_item for Parser<'_> {
    fn for_locking_item_1(&mut self, v1: i32, v2: List, v3: i32) -> Result<Option<Node>, Error> {
        let n = LockingClause {
            lockedRels: v2,
            strength: LockClauseStrength(v1),
            waitPolicy: LockWaitPolicy(v3),
        };
        Ok(Some(n.into()))
    }
}

impl rules::values_clause for Parser<'_> {
    fn values_clause_1(&mut self, v3: List) -> Result<Option<Node>, Error> {
        let n = SelectStmt { valuesLists: list_make1(listNode(v3)), ..SelectStmt::default() };
        Ok(Some(n.into()))
    }

    fn values_clause_2(&mut self, v1: Option<Node>, v4: List) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        let n: &mut SelectStmt = castMut(&mut v1)?;
        n.valuesLists.push(listNode(v4));
        Ok(v1)
    }
}

impl rules::table_ref for Parser<'_> {
    fn table_ref_1(
        &mut self,
        v1: Option<Box<RangeVar>>,
        v2: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        if let Some(r) = v1.as_mut() {
            r.alias = v2;
        }
        Ok(v1.map(NodeType::into_node))
    }

    fn table_ref_2(
        &mut self,
        v1: Option<Box<RangeVar>>,
        v2: Option<Box<Alias>>,
        v3: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        let (mut v1, mut v3) = (v1, v3);
        let n: &mut RangeTableSample = castMut(&mut v3)?;
        if let Some(r) = v1.as_mut() {
            r.alias = v2;
        }
        // The `relation_expr` goes inside the `RangeTableSample` node.
        n.relation = v1.map(NodeType::into_node);
        Ok(v3)
    }

    fn table_ref_3(&mut self, v1: Option<Node>, v2: List) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        func_alias(castMut(&mut v1)?, v2)?;
        Ok(v1)
    }

    fn table_ref_4(&mut self, v2: Option<Node>, v3: List) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        let n: &mut RangeFunction = castMut(&mut v2)?;
        n.lateral = true;
        func_alias(n, v3)?;
        Ok(v2)
    }

    fn table_ref_5(
        &mut self,
        v1: Option<Node>,
        v2: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        let n: &mut RangeTableFunc = castMut(&mut v1)?;
        n.alias = v2;
        Ok(v1)
    }

    fn table_ref_6(
        &mut self,
        v2: Option<Node>,
        v3: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        let n: &mut RangeTableFunc = castMut(&mut v2)?;
        n.lateral = true;
        n.alias = v3;
        Ok(v2)
    }

    fn table_ref_7(
        &mut self,
        v1: Option<Node>,
        v2: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let n = RangeSubselect { lateral: false, subquery: v1, alias: v2 };
        Ok(Some(n.into()))
    }

    fn table_ref_8(
        &mut self,
        v2: Option<Node>,
        v3: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let n = RangeSubselect { lateral: true, subquery: v2, alias: v3 };
        Ok(Some(n.into()))
    }

    fn table_ref_10(
        &mut self,
        v2: Option<Box<JoinExpr>>,
        v4: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        if let Some(j) = v2.as_mut() {
            j.alias = v4;
        }
        Ok(v2.map(NodeType::into_node))
    }

    fn table_ref_11(
        &mut self,
        v1: Option<Node>,
        v2: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        let jt: &mut JsonTable = castMut(&mut v1)?;
        jt.alias = v2;
        Ok(v1)
    }

    fn table_ref_12(
        &mut self,
        v2: Option<Node>,
        v3: Option<Box<Alias>>,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        let jt: &mut JsonTable = castMut(&mut v2)?;
        jt.alias = v3;
        jt.lateral = true;
        Ok(v2)
    }
}

/// Sets the alias and the column definitions of `func_alias_clause` on a `RangeFunction`.
fn func_alias(n: &mut RangeFunction, func_alias_clause: List) -> Result<(), Error> {
    let [alias, coldeflist] = elements(func_alias_clause);
    n.alias = castNode(alias)?;
    n.coldeflist = castList(coldeflist)?;
    Ok(())
}

/// A `JoinExpr` with an `ON` or a `USING` clause. A `USING` clause is a list of the column names
/// and the alias of the join.
fn join(
    jointype: JoinType,
    larg: Option<Node>,
    rarg: Option<Node>,
    join_qual: Option<Node>,
) -> Result<Option<Box<JoinExpr>>, Error> {
    let mut n = JoinExpr { jointype, isNatural: false, larg, rarg, ..JoinExpr::default() };
    match join_qual {
        Some(Node::List(qual)) => {
            let [usingClause, alias] = elements(qual);
            n.usingClause = castList(usingClause)?;
            n.join_using_alias = castNode(alias)?;
        }
        quals => n.quals = quals,
    }
    Ok(Some(Box::new(n)))
}

/// A `JoinExpr` of `CROSS JOIN` or `NATURAL JOIN`, with no qualification.
fn join_natural(
    jointype: JoinType,
    isNatural: bool,
    larg: Option<Node>,
    rarg: Option<Node>,
) -> Option<Box<JoinExpr>> {
    Some(Box::new(JoinExpr { jointype, isNatural, larg, rarg, ..JoinExpr::default() }))
}

impl rules::joined_table for Parser<'_> {
    fn joined_table_2(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
    ) -> Result<Option<Box<JoinExpr>>, Error> {
        // `CROSS JOIN` is the same as an inner join with no qualification.
        Ok(join_natural(JoinType::JOIN_INNER, false, v1, v4))
    }

    fn joined_table_3(
        &mut self,
        v1: Option<Node>,
        v2: JoinType,
        v4: Option<Node>,
        v5: Option<Node>,
    ) -> Result<Option<Box<JoinExpr>>, Error> {
        join(v2, v1, v4, v5)
    }

    fn joined_table_4(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v4: Option<Node>,
    ) -> Result<Option<Box<JoinExpr>>, Error> {
        // A `join_type` that can be empty does not work in the grammar.
        join(JoinType::JOIN_INNER, v1, v3, v4)
    }

    fn joined_table_5(
        &mut self,
        v1: Option<Node>,
        v3: JoinType,
        v5: Option<Node>,
    ) -> Result<Option<Box<JoinExpr>>, Error> {
        // The columns of a natural join are found later.
        Ok(join_natural(v3, true, v1, v5))
    }

    fn joined_table_6(
        &mut self,
        v1: Option<Node>,
        v4: Option<Node>,
    ) -> Result<Option<Box<JoinExpr>>, Error> {
        Ok(join_natural(JoinType::JOIN_INNER, true, v1, v4))
    }
}

/// An `Alias`.
fn alias(aliasname: Option<Str>, colnames: List) -> Option<Box<Alias>> {
    Some(Box::new(makeAlias(aliasname, colnames)))
}

impl rules::alias_clause for Parser<'_> {
    fn alias_clause_1(&mut self, v2: Option<Str>, v4: List) -> Result<Option<Box<Alias>>, Error> {
        Ok(alias(v2, v4))
    }

    fn alias_clause_2(&mut self, v2: Option<Str>) -> Result<Option<Box<Alias>>, Error> {
        Ok(alias(v2, List::new()))
    }

    fn alias_clause_3(&mut self, v1: Option<Str>, v3: List) -> Result<Option<Box<Alias>>, Error> {
        Ok(alias(v1, v3))
    }

    fn alias_clause_4(&mut self, v1: Option<Str>) -> Result<Option<Box<Alias>>, Error> {
        Ok(alias(v1, List::new()))
    }
}

impl rules::opt_alias_clause_for_join_using for Parser<'_> {
    fn opt_alias_clause_for_join_using_1(
        &mut self,
        v2: Option<Str>,
    ) -> Result<Option<Box<Alias>>, Error> {
        // The list of the column names comes later.
        Ok(alias(v2, List::new()))
    }
}

impl rules::func_alias_clause for Parser<'_> {
    fn func_alias_clause_3(&mut self, v2: Option<Str>, v4: List) -> Result<List, Error> {
        Ok(list_make2(Some(makeAlias(v2, List::new()).into()), listNode(v4)))
    }

    fn func_alias_clause_4(&mut self, v1: Option<Str>, v3: List) -> Result<List, Error> {
        Ok(list_make2(Some(makeAlias(v1, List::new()).into()), listNode(v3)))
    }
}

impl rules::join_qual for Parser<'_> {
    fn join_qual_1(&mut self, v3: List, v5: Option<Box<Alias>>) -> Result<Option<Node>, Error> {
        // `list_make2` is never `NIL`, so this is a `List` node also when both are `NULL`.
        Ok(Some(Node::List(list_make2(listNode(v3), v5.map(NodeType::into_node)))))
    }
}

/// Sets the inheritance of a relation and clears its alias.
fn inheritance(rel: Option<Box<RangeVar>>, inh: bool) -> Option<Box<RangeVar>> {
    let mut rel = rel;
    if let Some(r) = rel.as_mut() {
        r.inh = inh;
        r.alias = None;
    }
    rel
}

impl rules::relation_expr for Parser<'_> {
    fn relation_expr_1(
        &mut self,
        v1: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        // An inheritance query, implicitly.
        Ok(inheritance(v1, true))
    }
}

impl rules::extended_relation_expr for Parser<'_> {
    fn extended_relation_expr_1(
        &mut self,
        v1: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        // An inheritance query, explicitly.
        Ok(inheritance(v1, true))
    }

    fn extended_relation_expr_2(
        &mut self,
        v2: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        // No inheritance.
        Ok(inheritance(v2, false))
    }

    fn extended_relation_expr_3(
        &mut self,
        v3: Option<Box<RangeVar>>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        // No inheritance, in the syntax of SQL99.
        Ok(inheritance(v3, false))
    }
}

/// Sets the alias of a relation to `aliasname`.
fn relation_alias(rel: Option<Box<RangeVar>>, aliasname: Option<Str>) -> Option<Box<RangeVar>> {
    let mut rel = rel;
    if let Some(r) = rel.as_mut() {
        r.alias = alias(aliasname, List::new());
    }
    rel
}

impl rules::relation_expr_opt_alias for Parser<'_> {
    fn relation_expr_opt_alias_2(
        &mut self,
        v1: Option<Box<RangeVar>>,
        v2: Option<Str>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(relation_alias(v1, v2))
    }

    fn relation_expr_opt_alias_3(
        &mut self,
        v1: Option<Box<RangeVar>>,
        v3: Option<Str>,
    ) -> Result<Option<Box<RangeVar>>, Error> {
        Ok(relation_alias(v1, v3))
    }
}

impl rules::tablesample_clause for Parser<'_> {
    fn tablesample_clause_1(
        &mut self,
        v2: List,
        v4: List,
        v6: Option<Node>,
        at2: i32,
    ) -> Result<Option<Node>, Error> {
        // The relation is set by `table_ref`.
        let n = RangeTableSample {
            relation: None,
            method: v2,
            args: v4,
            repeatable: v6,
            location: at2,
        };
        Ok(Some(n.into()))
    }
}

impl rules::func_table for Parser<'_> {
    fn func_table_1(&mut self, v1: Option<Node>, v2: bool) -> Result<Option<Node>, Error> {
        // The alias and the column definitions are set by `table_ref`.
        let n = RangeFunction {
            lateral: false,
            ordinality: v2,
            is_rowsfrom: false,
            functions: list_make1(Some(Node::List(list_make2(v1, None)))),
            ..RangeFunction::default()
        };
        Ok(Some(n.into()))
    }

    fn func_table_2(&mut self, v4: List, v6: bool) -> Result<Option<Node>, Error> {
        let n = RangeFunction {
            lateral: false,
            ordinality: v6,
            is_rowsfrom: true,
            functions: v4,
            ..RangeFunction::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::where_or_current_clause for Parser<'_> {
    fn where_or_current_clause_2(&mut self, v4: Option<Str>) -> Result<Option<Node>, Error> {
        // The parse analysis sets `cvarno`.
        let n = CurrentOfExpr { cvarno: 0, cursor_name: v4, cursor_param: 0 };
        Ok(Some(n.into()))
    }
}

impl rules::TableFuncElement for Parser<'_> {
    fn TableFuncElement_1(
        &mut self,
        v1: Option<Str>,
        v2: Option<Box<TypeName>>,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = ColumnDef {
            colname: v1,
            typeName: v2,
            inhcount: 0,
            is_local: true,
            is_not_null: false,
            is_from_type: false,
            storage: 0,
            raw_default: None,
            cooked_default: None,
            collClause: castNode(v3)?,
            collOid: 0,
            constraints: List::new(),
            location: at1,
            ..ColumnDef::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::xmltable for Parser<'_> {
    fn xmltable_1(
        &mut self,
        v3: Option<Node>,
        v4: Option<Node>,
        v6: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = RangeTableFunc {
            rowexpr: v3,
            docexpr: v4,
            columns: v6,
            namespaces: List::new(),
            location: at1,
            ..RangeTableFunc::default()
        };
        Ok(Some(n.into()))
    }

    fn xmltable_2(
        &mut self,
        v5: List,
        v8: Option<Node>,
        v9: Option<Node>,
        v11: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let n = RangeTableFunc {
            rowexpr: v8,
            docexpr: v9,
            columns: v11,
            namespaces: v5,
            location: at1,
            ..RangeTableFunc::default()
        };
        Ok(Some(n.into()))
    }
}

impl rules::PLpgSQL_Expr for Parser<'_> {
    #[allow(clippy::too_many_arguments)]
    fn PLpgSQL_Expr_1(
        &mut self,
        v1: List,
        v2: List,
        v3: List,
        v4: Option<Node>,
        v5: Option<Box<GroupClause>>,
        v6: Option<Node>,
        v7: List,
        v8: List,
        v9: Option<Box<SelectLimit>>,
        v10: List,
    ) -> Result<Option<Node>, Error> {
        // A `SELECT` with no `SELECT` keyword and no `INTO`, as PL/pgSQL gives its expressions.
        let mut n = select(v1, v2, None, v3, v4, v5, v6, v7);
        let s: &mut SelectStmt = castMut(&mut n)?;
        s.sortClause = v8;
        if let Some(limit) = v9 {
            s.limitOffset = limit.limitOffset;
            s.limitCount = limit.limitCount;
            if s.sortClause.is_empty() && limit.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES {
                let message = "WITH TIES cannot be specified without ORDER BY clause";
                return Err(self.error(ERRCODE_SYNTAX_ERROR, message, limit.optionLoc));
            }
            s.limitOption = limit.limitOption;
        }
        s.lockingClause = v10;
        Ok(n)
    }
}

impl rules::PLAssignStmt for Parser<'_> {
    fn PLAssignStmt_1(
        &mut self,
        v1: Option<Str>,
        v2: List,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // The rule that calls this one sets `nnames`.
        let n = PLAssignStmt {
            name: v1,
            indirection: check_indirection(v2, self)?,
            nnames: 0,
            val: castNode::<SelectStmt>(v4)?,
            location: at1,
        };
        Ok(Some(n.into()))
    }
}

impl rules::plassign_target for Parser<'_> {
    fn plassign_target_2(&mut self, v1: i32) -> Result<Option<Str>, Error> {
        Ok(Some(format!("${v1}").into()))
    }
}
