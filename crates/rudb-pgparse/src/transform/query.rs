//! Queries: the select, the set operations, `VALUES`, `WITH`, the `FROM` items and the windows.

use rudb_common::{Error, Span, SqlState};
use rudb_parse::NONE;
use rudb_parse::ast::{
    Cte, Distinct, JoinKind, Nulls, Order, OrderItem, Quantifier, Query, QueryBody, QueryRef,
    Select, SetOp, Slice, Source, SourceRef, Target, WindowBound, WindowExclude, WindowRef,
    WindowSpec, WindowUnit,
};

use super::{Definition, Made, Transform, clause, not_yet};
use crate::nodes::{
    Alias, CTEMaterialize, CommonTableExpr, FRAMEOPTION_BETWEEN, FRAMEOPTION_DEFAULTS,
    FRAMEOPTION_EXCLUDE_CURRENT_ROW, FRAMEOPTION_EXCLUDE_GROUP, FRAMEOPTION_EXCLUDE_TIES,
    FRAMEOPTION_GROUPS, FRAMEOPTION_NONDEFAULT, FRAMEOPTION_ROWS, FRAMEOPTION_START_CURRENT_ROW,
    FRAMEOPTION_START_OFFSET_FOLLOWING, FRAMEOPTION_START_OFFSET_PRECEDING,
    FRAMEOPTION_START_UNBOUNDED_FOLLOWING, FRAMEOPTION_START_UNBOUNDED_PRECEDING, JoinExpr,
    JoinType, LimitOption, List, Node, RangeFunction, RangeVar, SelectStmt, SetOperation, SortBy,
    SortByDir, SortByNulls, WindowDef, WithClause,
};

impl Transform<'_> {
    /// A query that is a statement or a subquery, one level deeper than the query around it.
    pub(super) fn query(&mut self, select: &SelectStmt) -> Made<QueryRef> {
        self.depth += 1;
        let made = self.query_inner(select);
        self.depth -= 1;
        made
    }

    /// A query at the depth of the query being transformed, which is what a leg of a set operation
    /// with no `WITH` of its own is.
    fn query_inner(&mut self, select: &SelectStmt) -> Made<QueryRef> {
        let mark = self.scope.len();
        if let Some(with) = &select.withClause {
            self.with_clause(with)?;
        }
        // The names of a `WINDOW` clause are seen only by the select that writes them, and by its
        // `ORDER BY`. A subquery does not see them, which is how PostgreSQL reads them.
        let outer = std::mem::take(&mut self.windows);
        let made = self.query_rest(select);
        self.windows = outer;
        let mut query = made?;
        let once = self.settle(mark);
        query.ctes = self.ast.cte_slice(once);
        let span = self.span;
        Ok(self.ast.push_query(query, span))
    }

    /// The body of a query, its `ORDER BY` and its limits.
    fn query_rest(&mut self, select: &SelectStmt) -> Made<Query> {
        let body = self.body(select)?;
        let mut query = Query::bare(body);
        let mut items = Vec::with_capacity(select.sortClause.len());
        for node in select.sortClause.iter().flatten() {
            items.push(self.sort_by(node)?);
        }
        query.order_by = self.ast.order_slice(items);
        if select.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES {
            return clause("WithTies");
        }
        query.limit = self.limit(select.limitCount.as_ref())?;
        query.offset = self.limit(select.limitOffset.as_ref())?;
        Ok(query)
    }

    /// `LIMIT` or `OFFSET`. `LIMIT ALL` is a null constant, which is no limit.
    fn limit(&mut self, node: Option<&Node>) -> Made<u32> {
        match node {
            None => Ok(NONE),
            Some(Node::A_Const(constant)) if constant.isnull => Ok(NONE),
            Some(node) => self.expr(node),
        }
    }

    fn body(&mut self, select: &SelectStmt) -> Made<QueryBody> {
        if select.intoClause.is_some() {
            return clause("IntoClause");
        }
        if !select.lockingClause.is_empty() {
            return clause("LockingClause");
        }
        let op = match select.op {
            SetOperation::SETOP_NONE if !select.valuesLists.is_empty() => {
                return self.values(&select.valuesLists);
            }
            SetOperation::SETOP_NONE => return Ok(QueryBody::Select(self.select(select)?)),
            SetOperation::SETOP_UNION => SetOp::Union,
            SetOperation::SETOP_INTERSECT => SetOp::Intersect,
            _ => SetOp::Except,
        };
        let (Some(left), Some(right)) = (&select.larg, &select.rarg) else {
            return clause("SelectStmt");
        };
        let left = self.leg(left)?;
        let right = self.leg(right)?;
        let quantifier = if select.all { Quantifier::All } else { Quantifier::Unstated };
        Ok(QueryBody::SetOp { op, quantifier, by_name: false, left, right })
    }

    /// One side of a set operation. A side with a `WITH` of its own is a query one level deeper,
    /// as a subquery is.
    fn leg(&mut self, select: &SelectStmt) -> Made<QueryRef> {
        if select.withClause.is_some() { self.query(select) } else { self.query_inner(select) }
    }

    fn values(&mut self, lists: &List) -> Made<QueryBody> {
        let mut rows = Vec::with_capacity(lists.len());
        for row in lists.iter().flatten() {
            let Node::List(items) = row else {
                return Err(not_yet(row));
            };
            rows.push(self.expr_list(items)?);
        }
        Ok(QueryBody::Values(self.ast.row_slice(rows)))
    }

    fn select(&mut self, select: &SelectStmt) -> Made<u32> {
        for node in select.windowClause.iter().flatten() {
            let Node::WindowDef(def) = node else {
                return Err(not_yet(node));
            };
            let name = def.name.as_deref().unwrap_or_default();
            if self.windows.iter().any(|(defined, _, _)| defined == name) {
                return Err(Error::parser(format!("window \"{name}\" is already defined"))
                    .state(SqlState::WINDOWING_ERROR)
                    .with_span(self.at(def.location))
                    .into());
            }
            let spec = self.window_def(def)?;
            let framed = def.frameOptions != FRAMEOPTION_DEFAULTS;
            self.windows.push((name.to_string(), spec, framed));
        }
        let mut from = Vec::with_capacity(select.fromClause.len());
        for node in select.fromClause.iter().flatten() {
            from.push(self.source(node)?);
        }
        let from = self.ast.source_slice(from);
        let distinct = match select.distinctClause.as_slice() {
            [] => Distinct::No,
            [None] => Distinct::Yes,
            list => Distinct::On(self.expr_list(list)?),
        };
        let targets = self.targets(&select.targetList)?;
        let filter = self.optional(select.whereClause.as_ref())?;
        if select.groupDistinct {
            return clause("GroupDistinct");
        }
        if let Some(node) =
            select.groupClause.iter().flatten().find(|node| matches!(node, Node::GroupingSet(_)))
        {
            return Err(not_yet(node));
        }
        let group_by = self.expr_list(&select.groupClause)?;
        let having = self.optional(select.havingClause.as_ref())?;
        Ok(self.ast.push_select(Select {
            distinct,
            targets,
            from,
            filter,
            group_by,
            having,
            ..Select::empty()
        }))
    }

    pub(super) fn sort_by(&mut self, node: &Node) -> Made<OrderItem> {
        let Node::SortBy(sort) = node else {
            return Err(not_yet(node));
        };
        let SortBy { node, sortby_dir, sortby_nulls, .. } = &**sort;
        let order = match *sortby_dir {
            SortByDir::SORTBY_ASC => Order::Ascending,
            SortByDir::SORTBY_DESC => Order::Descending,
            SortByDir::SORTBY_DEFAULT => Order::Unstated,
            _ => return clause("SortByUsing"),
        };
        let nulls = match *sortby_nulls {
            SortByNulls::SORTBY_NULLS_FIRST => Nulls::First,
            SortByNulls::SORTBY_NULLS_LAST => Nulls::Last,
            _ => Nulls::Unstated,
        };
        let Some(node) = node else {
            return clause("SortBy");
        };
        Ok(OrderItem { expr: self.expr(node)?, order, nulls })
    }

    /// A target list, of a select or of a `RETURNING`.
    pub(super) fn targets(&mut self, list: &List) -> Made<Slice> {
        let mut targets = Vec::with_capacity(list.len());
        for node in list.iter().flatten() {
            let Node::ResTarget(target) = node else {
                return Err(not_yet(node));
            };
            if !target.indirection.is_empty() {
                return clause("ResTarget");
            }
            let Some(value) = &target.val else {
                return clause("ResTarget");
            };
            let expr = self.expr(value)?;
            let alias = match &target.name {
                Some(name) => self.intern(name),
                None => NONE,
            };
            targets.push(Target { expr, alias });
        }
        Ok(self.ast.target_slice(targets))
    }

    // `WITH`.

    /// The definitions of a `WITH`, put in scope for the rest of the query.
    pub(super) fn with_clause(&mut self, with: &WithClause) -> Made<()> {
        let mark = self.scope.len();
        for node in with.ctes.iter().flatten() {
            let Node::CommonTableExpr(cte) = node else {
                return Err(not_yet(node));
            };
            let name = cte.ctename.as_deref().unwrap_or_default();
            if self.scope[mark..].iter().any(|defined| defined.name == name) {
                return Err(Error::parser(format!(
                    "WITH query name \"{name}\" specified more than once"
                ))
                .state(SqlState::DUPLICATE_ALIAS)
                .with_span(self.at(cte.location))
                .into());
            }
            if cte.search_clause.is_some() {
                return clause("CTESearchClause");
            }
            if cte.cycle_clause.is_some() {
                return clause("CTECycleClause");
            }
            let select = match &cte.ctequery {
                Some(Node::SelectStmt(select)) => select,
                Some(node) => return Err(not_yet(node)),
                None => return clause("CommonTableExpr"),
            };
            let declared = self.names(&cte.aliascolnames)?;
            self.defined.push(name.to_string());
            let definition = if with.recursive {
                self.recursive_definition(cte, select, declared)?
            } else {
                let query = self.query(select)?;
                Definition {
                    name: name.to_string(),
                    declared,
                    materialized: cte.ctematerialized,
                    query,
                    slot: NONE,
                    recursive: false,
                    logged: 0,
                    reads: Vec::new(),
                }
            };
            self.scope.push(Definition { logged: self.defined.len(), ..definition });
        }
        Ok(())
    }

    /// A definition under `WITH RECURSIVE`, with its own name in scope while its query is read.
    ///
    /// PostgreSQL checks the form of a definition that reads itself when it analyzes the query, and
    /// the errors are given here with its messages. The query is a `UNION` or a `UNION ALL`. Its
    /// left side, the non-recursive term, does not read the name. And the query has no `ORDER BY`,
    /// `OFFSET` or `LIMIT` of its own.
    fn recursive_definition(
        &mut self,
        cte: &CommonTableExpr,
        select: &SelectStmt,
        declared: Slice,
    ) -> Made<Definition> {
        let name = cte.ctename.as_deref().unwrap_or_default();
        let slot = self.ast.ctes.len() as u32;
        let interned = self.intern(name);
        self.ast.ctes.push(Cte {
            name: interned,
            query: 0,
            columns: declared,
            recursive: false,
            key: Slice::default(),
            dml: None,
        });
        self.scope.push(Definition {
            name: name.to_string(),
            declared,
            materialized: cte.ctematerialized,
            query: NONE,
            slot,
            recursive: true,
            logged: 0,
            reads: Vec::new(),
        });
        let reads = self.self_reads.len();
        self.recursing.push(slot);
        let query = self.query(select);
        self.recursing.pop();
        self.scope.pop();
        let query = query?;
        let found: Vec<(Span, u32)> = self
            .self_reads
            .drain(reads..)
            .filter(|&(read, ..)| read == slot)
            .map(|(_, span, made)| (span, made))
            .collect();
        if let Some(&(span, _)) = found.first() {
            let written = self.ast.queries[query as usize];
            let QueryBody::SetOp { op: SetOp::Union, left, .. } = written.body else {
                return Err(Error::parser(format!(
                    "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
                ))
                .state(SqlState::INVALID_RECURSION)
                .with_span(self.at(cte.location))
                .into());
            };
            if let Some(&(span, _)) = found.iter().find(|&&(_, made)| made <= left) {
                return Err(Error::parser(format!(
                    "recursive reference to query \"{name}\" must not appear within its non-recursive term"
                ))
                .state(SqlState::INVALID_RECURSION)
                .with_span(span)
                .into());
            }
            let refused = if !written.order_by.is_empty() {
                Some("ORDER BY")
            } else if written.offset != NONE {
                Some("OFFSET")
            } else if written.limit != NONE {
                Some("LIMIT")
            } else {
                None
            };
            if let Some(refused) = refused {
                return Err(Error::parser(format!(
                    "{refused} in a recursive query is not implemented"
                ))
                .state(SqlState::FEATURE_NOT_SUPPORTED)
                .with_span(span)
                .into());
            }
        }
        let recursive = !found.is_empty();
        self.ast.ctes[slot as usize].query = query;
        self.ast.ctes[slot as usize].recursive = recursive;
        Ok(Definition {
            name: name.to_string(),
            declared,
            materialized: cte.ctematerialized,
            query,
            slot,
            recursive,
            logged: 0,
            reads: Vec::new(),
        })
    }

    /// Takes the definitions of the query out of scope, and gives the ones that are held, which
    /// the query carries.
    ///
    /// A held definition runs once and each read of it reads its rows. Any other definition is put
    /// into each place that reads it. The rule is the one of the DuckDB transform, so the two
    /// dialects plan a query the same way. A definition is held when `MATERIALIZED` asks for it,
    /// when it reads itself, or when it is read more than once by the statement's own query and no
    /// definition after it takes its name. `NOT MATERIALIZED` puts a definition in place even when
    /// it is read more than once.
    pub(super) fn settle(&mut self, mark: usize) -> Vec<u32> {
        let definitions = self.scope.split_off(mark);
        let mut once = Vec::new();
        for definition in definitions {
            let reads = definition.reads.len();
            let asked = definition.materialized == CTEMaterialize::CTEMaterializeAlways;
            let refused = definition.materialized == CTEMaterialize::CTEMaterializeNever;
            let hidden = self.defined[definition.logged..].contains(&definition.name);
            let worth = self.depth == 1 && !hidden && reads > 1;
            let held = (definition.recursive && reads > 0) || asked || (!refused && worth);
            if !held {
                continue;
            }
            let cte = if definition.slot == NONE {
                let name = self.intern(&definition.name);
                self.ast.ctes.push(Cte {
                    name,
                    query: definition.query,
                    columns: definition.declared,
                    recursive: false,
                    key: Slice::default(),
                    dml: None,
                });
                self.ast.ctes.len() as u32 - 1
            } else {
                definition.slot
            };
            for (source, alias, columns) in definition.reads {
                self.ast.sources[source as usize] =
                    Source::Cte { cte, alias, columns, recurring: false };
            }
            once.push(cte);
        }
        once
    }

    // `FROM`.

    pub(super) fn source(&mut self, node: &Node) -> Made<SourceRef> {
        match node {
            Node::RangeVar(table) => self.table(table),
            Node::JoinExpr(join) => self.join(join),
            Node::RangeSubselect(subselect) => {
                let Some(Node::SelectStmt(select)) = &subselect.subquery else {
                    return clause("RangeSubselect");
                };
                let query = self.query(select)?;
                let (alias, columns) = self.alias(subselect.alias.as_deref())?;
                Ok(self.ast.push_source(Source::Subquery { query, alias, columns }, self.span))
            }
            Node::RangeFunction(function) => self.function_source(function),
            node => Err(not_yet(node)),
        }
    }

    fn alias(&mut self, alias: Option<&Alias>) -> Made<(u32, Slice)> {
        let Some(alias) = alias else {
            return Ok((NONE, Slice::default()));
        };
        let name = match &alias.aliasname {
            Some(name) => self.intern(name),
            None => NONE,
        };
        Ok((name, self.names(&alias.colnames)?))
    }

    /// A table, or a read of a `WITH` definition when the name has one part and a definition in
    /// scope has that name.
    fn table(&mut self, table: &RangeVar) -> Made<SourceRef> {
        let (alias, columns) = self.alias(table.alias.as_deref())?;
        let span = self.at(table.location);
        let parts: Vec<&str> = [&table.catalogname, &table.schemaname, &table.relname]
            .into_iter()
            .filter_map(|part| part.as_deref())
            .collect();
        if let [name] = parts[..]
            && let Some(at) = self.scope.iter().rposition(|definition| definition.name == name)
        {
            let definition = &self.scope[at];
            if definition.query == NONE {
                let cte = definition.slot;
                let made = self.ast.queries.len() as u32;
                self.self_reads.push((cte, span, made));
                let recurring = false;
                return Ok(self
                    .ast
                    .push_source(Source::Cte { cte, alias, columns, recurring }, span));
            }
            let (query, declared) = (definition.query, definition.declared);
            let named = if alias == NONE { self.intern(name) } else { alias };
            let shown = if columns.is_empty() { declared } else { columns };
            let source = self
                .ast
                .push_source(Source::Subquery { query, alias: named, columns: shown }, span);
            self.scope[at].reads.push((source, alias, columns));
            return Ok(source);
        }
        let parts: Vec<u32> = parts.into_iter().map(|part| self.intern(part)).collect();
        let name = self.ast.part_slice(parts);
        Ok(self.ast.push_source(Source::Table { name, alias, columns }, span))
    }

    fn join(&mut self, join: &JoinExpr) -> Made<SourceRef> {
        if join.alias.is_some() || join.join_using_alias.is_some() {
            return clause("JoinAlias");
        }
        let (Some(left), Some(right)) = (&join.larg, &join.rarg) else {
            return clause("JoinExpr");
        };
        let left = self.source(left)?;
        let right = self.source(right)?;
        let using = self.names(&join.usingClause)?;
        let on = self.optional(join.quals.as_ref())?;
        let kind = match join.jointype {
            JoinType::JOIN_INNER if on == NONE && !join.isNatural && using.is_empty() => {
                JoinKind::Cross
            }
            JoinType::JOIN_INNER => JoinKind::Inner,
            JoinType::JOIN_LEFT => JoinKind::Left,
            JoinType::JOIN_RIGHT => JoinKind::Right,
            JoinType::JOIN_FULL => JoinKind::Full,
            _ => return clause("JoinExpr"),
        };
        let natural = join.isNatural;
        let join = Source::Join { left, right, kind, natural, on, using };
        Ok(self.ast.push_source(join, self.span))
    }

    /// A function in `FROM`. `WITH ORDINALITY`, `ROWS FROM` and a column definition list are not
    /// built yet.
    fn function_source(&mut self, function: &RangeFunction) -> Made<SourceRef> {
        if function.ordinality {
            return clause("WithOrdinality");
        }
        if function.is_rowsfrom || function.functions.len() != 1 || !function.coldeflist.is_empty()
        {
            return clause("RowsFrom");
        }
        let Some(Node::List(pair)) = &function.functions[0] else {
            return clause("RangeFunction");
        };
        if pair.get(1).is_some_and(Option::is_some) {
            return clause("ColumnDefList");
        }
        let call = match pair.first() {
            Some(Some(Node::FuncCall(call))) => call,
            Some(Some(node)) => return Err(not_yet(node)),
            _ => return clause("RangeFunction"),
        };
        if call.agg_star
            || call.agg_distinct
            || call.agg_within_group
            || call.func_variadic
            || call.over.is_some()
            || call.agg_filter.is_some()
            || !call.agg_order.is_empty()
            || call.funcformat != crate::nodes::CoercionForm::COERCE_EXPLICIT_CALL
        {
            return clause("FuncCall");
        }
        let name = self.names(&call.funcname)?;
        let mut args = Vec::with_capacity(call.args.len());
        for node in call.args.iter().flatten() {
            args.push(match node {
                Node::NamedArgExpr(named) => {
                    let Some(value) = &named.arg else {
                        return clause("NamedArgExpr");
                    };
                    let expr = self.expr(value)?;
                    let alias = self.intern(named.name.as_deref().unwrap_or_default());
                    Target { expr, alias }
                }
                node => Target { expr: self.expr(node)?, alias: NONE },
            });
        }
        let args = self.ast.target_slice(args);
        let (alias, columns) = self.alias(function.alias.as_deref())?;
        let function = Source::Function { name, args, alias, columns, pragma: false };
        Ok(self.ast.push_source(function, self.span))
    }

    // Windows.

    /// The window of an `OVER`: a name from the `WINDOW` clause, or a spec in parentheses.
    pub(super) fn over(&mut self, def: &WindowDef) -> Made<WindowRef> {
        let Some(name) = &def.name else {
            return self.window_def(def);
        };
        match self.windows.iter().find(|(defined, _, _)| defined == &**name) {
            Some(&(_, spec, _)) => Ok(spec),
            None => Err(missing_window(name, self.at(def.location))),
        }
    }

    /// A window spec, from the `WINDOW` clause or from the parentheses of an `OVER`.
    ///
    /// A spec that names another window copies it. The copy can add an `ORDER BY` and a frame,
    /// and the rules of PostgreSQL say what else it can do, with its messages.
    fn window_def(&mut self, def: &WindowDef) -> Made<WindowRef> {
        let mut spec = WindowSpec::empty();
        if let Some(base) = &def.refname {
            let span = self.at(def.location);
            let Some(&(_, found, framed)) =
                self.windows.iter().find(|(defined, _, _)| defined == &**base)
            else {
                return Err(missing_window(base, span));
            };
            let copied = self.ast.windows[found as usize];
            if !def.partitionClause.is_empty() {
                return Err(windowing(format!(
                    "cannot override PARTITION BY clause of window \"{base}\""
                ))
                .with_span(span)
                .into());
            }
            if !def.orderClause.is_empty() && !copied.order.is_empty() {
                return Err(windowing(format!(
                    "cannot override ORDER BY clause of window \"{base}\""
                ))
                .with_span(span)
                .into());
            }
            if framed {
                let error = windowing(format!(
                    "cannot copy window \"{base}\" because it has a frame clause"
                ))
                .with_span(span);
                let bare = def.name.is_none()
                    && def.orderClause.is_empty()
                    && def.frameOptions == FRAMEOPTION_DEFAULTS;
                let error = if bare {
                    error.hint("Omit the parentheses in this OVER clause.")
                } else {
                    error
                };
                return Err(error.into());
            }
            spec.partition = copied.partition;
            spec.order = copied.order;
        } else {
            spec.partition = self.expr_list(&def.partitionClause)?;
        }
        if !def.orderClause.is_empty() {
            let mut items = Vec::with_capacity(def.orderClause.len());
            for node in def.orderClause.iter().flatten() {
                items.push(self.sort_by(node)?);
            }
            spec.order = self.ast.order_slice(items);
        }
        self.frame(&mut spec, def)?;
        Ok(self.ast.push_window(spec))
    }

    /// The frame of a window from the bits of `frameOptions`. The bits of the end are the bits of
    /// the start moved one place to the left, so one decoder reads both.
    fn frame(&mut self, spec: &mut WindowSpec, def: &WindowDef) -> Made<()> {
        let options = def.frameOptions;
        if options & FRAMEOPTION_NONDEFAULT == 0 {
            return Ok(());
        }
        spec.unit = if options & FRAMEOPTION_ROWS != 0 {
            WindowUnit::Rows
        } else if options & FRAMEOPTION_GROUPS != 0 {
            WindowUnit::Groups
        } else {
            WindowUnit::Range
        };
        spec.start = self.bound(options, def.startOffset.as_ref())?;
        spec.end = if options & FRAMEOPTION_BETWEEN == 0 {
            WindowBound::CurrentRow
        } else {
            self.bound(options >> 1, def.endOffset.as_ref())?
        };
        spec.exclude = if options & FRAMEOPTION_EXCLUDE_CURRENT_ROW != 0 {
            WindowExclude::CurrentRow
        } else if options & FRAMEOPTION_EXCLUDE_GROUP != 0 {
            WindowExclude::Group
        } else if options & FRAMEOPTION_EXCLUDE_TIES != 0 {
            WindowExclude::Ties
        } else {
            WindowExclude::NoOthers
        };
        // A frame from the first row to the last is the same frame in each unit, and the DuckDB
        // transform writes it as `ROWS`, so this one does too.
        if spec.start == WindowBound::UnboundedPreceding
            && spec.end == WindowBound::UnboundedFollowing
        {
            spec.unit = WindowUnit::Rows;
        }
        Ok(())
    }

    fn bound(&mut self, options: i32, offset: Option<&Node>) -> Made<WindowBound> {
        let offset = |transform: &mut Self| match offset {
            Some(node) => transform.expr(node),
            None => clause("WindowDef"),
        };
        Ok(if options & FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0 {
            WindowBound::UnboundedPreceding
        } else if options & FRAMEOPTION_START_UNBOUNDED_FOLLOWING != 0 {
            WindowBound::UnboundedFollowing
        } else if options & FRAMEOPTION_START_CURRENT_ROW != 0 {
            WindowBound::CurrentRow
        } else if options & FRAMEOPTION_START_OFFSET_PRECEDING != 0 {
            WindowBound::Preceding(offset(self)?)
        } else if options & FRAMEOPTION_START_OFFSET_FOLLOWING != 0 {
            WindowBound::Following(offset(self)?)
        } else {
            return clause("WindowDef");
        })
    }
}

fn windowing(message: String) -> Error {
    Error::parser(message).state(SqlState::WINDOWING_ERROR)
}

fn missing_window(name: &str, span: Span) -> super::Refused {
    Error::parser(format!("window \"{name}\" does not exist"))
        .state(SqlState::UNDEFINED_OBJECT)
        .with_span(span)
        .into()
}
