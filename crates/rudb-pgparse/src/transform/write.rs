//! The statements that write rows: `INSERT`, `UPDATE`, `DELETE` and `TRUNCATE`.
//!
//! The binder reads an `UPDATE`, a `DELETE` and the `DO UPDATE` of an `INSERT` as queries over the
//! table that they write. `rudb_parse::build` makes those queries for both transforms, so this file
//! only reads the parts of each statement out of the raw tree.

use rudb_common::{Error, Span, SqlState};
use rudb_parse::NONE;
use rudb_parse::ast::{
    Conflict, ConflictAction, Cte, ExprRef, Insert, Overriding, QueryRef, Slice, Statement, StrRef,
    Truncate,
};
use rudb_parse::build::{Assignment, Change};

use super::query::SelfRead;
use super::{Definition, Made, Transform, clause, not_yet};
use crate::nodes::{
    CommonTableExpr, DeleteStmt, DropBehavior, InsertStmt, List, MultiAssignRef, Node,
    OnConflictAction, OnConflictClause, OverridingKind, RangeVar, ReturningClause, SubLinkType,
    TruncateStmt, UpdateStmt, WithClause,
};

impl Transform<'_> {
    /// A `WITH` definition that is an `INSERT`, an `UPDATE` or a `DELETE`, with the checks of
    /// `transformWithClause` and `checkWellFormedRecursion` in `parse_cte.c`.
    ///
    /// PostgreSQL takes one only in the `WITH` of the statement itself. Under `WITH RECURSIVE` the
    /// name is in scope in the statement, and a statement that reads it is refused. The definition
    /// has its slot in `Ast::ctes` before the statement is read, and [`Self::settle`] holds it
    /// whether it is read or not, since it runs once in either case.
    pub(super) fn written_definition(
        &mut self,
        cte: &CommonTableExpr,
        written: &Node,
        declared: Slice,
        recursive: bool,
    ) -> Made<Definition> {
        let name = cte.ctename.as_deref().unwrap_or_default();
        let at = self.at(cte.location);
        if self.depth > 1 {
            return Err(Error::parser(
                "WITH clause containing a data-modifying statement must be at the top level",
            )
            .state(SqlState::FEATURE_NOT_SUPPORTED)
            .with_span(at)
            .into());
        }
        let slot = self.ast.ctes.len() as u32;
        let interned = self.intern(name);
        self.ast.ctes.push(Cte {
            name: interned,
            query: NONE,
            columns: declared,
            recursive: false,
            key: Slice::default(),
            dml: None,
            search: None,
            cycle: None,
        });
        let materialized = cte.ctematerialized;
        let mark = self.scope.len();
        if recursive {
            // A read of the name in the statement is a read of the definition itself.
            self.scope.push(Definition {
                name: name.to_string(),
                declared,
                materialized,
                query: NONE,
                slot,
                recursive: true,
                reads: Vec::new(),
                dml: None,
                span: at,
            });
        }
        let reads = self.self_reads.len();
        // The statement is a query below the top level, where a `WITH` of its own cannot write.
        self.depth += 1;
        let made = match written {
            Node::InsertStmt(insert) => self.insert(insert),
            Node::UpdateStmt(update) => self.update(update),
            Node::DeleteStmt(delete) => self.delete(delete),
            node => Err(not_yet(node)),
        };
        self.depth -= 1;
        self.scope.truncate(mark);
        let statement = made?;
        let (found, others): (Vec<SelfRead>, Vec<SelfRead>) =
            self.self_reads.split_off(reads).into_iter().partition(|read| read.cte == slot);
        self.self_reads.extend(others.into_iter().map(|read| SelfRead { nested: true, ..read }));
        if !found.is_empty() {
            return Err(Error::parser(format!(
                "recursive query \"{name}\" must not contain data-modifying statements"
            ))
            .state(SqlState::INVALID_RECURSION)
            .with_span(at)
            .into());
        }
        let (Statement::Insert(index) | Statement::Update(index) | Statement::Delete(index)) =
            statement
        else {
            return clause("CommonTableExpr");
        };
        let query = self.ast.inserts[index as usize].source;
        let held = &mut self.ast.ctes[slot as usize];
        held.query = query;
        held.dml = Some(statement);
        Ok(Definition {
            name: name.to_string(),
            declared,
            materialized,
            query,
            slot,
            recursive: false,
            reads: Vec::new(),
            dml: Some(statement),
            span: at,
        })
    }

    /// A writing statement, with the definitions of its `WITH` in scope.
    ///
    /// The definitions are settled at the depth of the statement, which is no query, so only a
    /// definition that `MATERIALIZED` asks for or that reads itself is held. This is the rule of
    /// the DuckDB transform for a writing statement. A held definition is carried by each query
    /// of the statement, because the binder binds each of them on its own.
    fn written_with(
        &mut self,
        with: Option<&WithClause>,
        build: impl FnOnce(&mut Self) -> Made<Statement>,
    ) -> Made<Statement> {
        let mark = self.scope.len();
        if let Some(with) = with {
            self.with_clause(with)?;
        }
        let made = build(self);
        let (once, defined) = self.settle(mark);
        let statement = made?;
        self.ast.carry_definitions(statement, &once);
        let owner = self.ast.carrier(statement);
        self.own(owner, defined);
        Ok(statement)
    }

    pub(super) fn insert(&mut self, insert: &InsertStmt) -> Made<Statement> {
        self.written_with(insert.withClause.as_deref(), |this| this.insert_body(insert))
    }

    pub(super) fn update(&mut self, update: &UpdateStmt) -> Made<Statement> {
        self.written_with(update.withClause.as_deref(), |this| {
            let (name, alias) = this.written_table(update.relation.as_deref())?;
            let sets = this.sets(&update.targetList)?;
            let returning = this.returning(update.returningClause.as_deref(), name, alias)?;
            let using = this.using(&update.fromClause)?;
            let filter = this.optional(update.whereClause.as_ref())?;
            let change = Change {
                name,
                alias,
                sets,
                filter,
                using,
                returning,
                delete: false,
                truncate: None,
            };
            Ok(this.changed_rows(change))
        })
    }

    pub(super) fn delete(&mut self, delete: &DeleteStmt) -> Made<Statement> {
        self.written_with(delete.withClause.as_deref(), |this| {
            let (name, alias) = this.written_table(delete.relation.as_deref())?;
            let returning = this.returning(delete.returningClause.as_deref(), name, alias)?;
            let using = this.using(&delete.usingClause)?;
            let filter = this.optional(delete.whereClause.as_ref())?;
            let sets = Vec::new();
            let change = Change {
                name,
                alias,
                sets,
                filter,
                using,
                returning,
                delete: true,
                truncate: None,
            };
            Ok(this.changed_rows(change))
        })
    }

    /// A `TRUNCATE`, as one `DELETE` with no condition for each table, in the order that the
    /// statement names them. Each of them has the options of the statement.
    pub(super) fn truncate(&mut self, truncate: &TruncateStmt) -> Made<Vec<Statement>> {
        let options = Truncate {
            restart: truncate.restart_seqs,
            cascade: truncate.behavior == DropBehavior::DROP_CASCADE,
        };
        let mut statements = Vec::with_capacity(truncate.relations.len());
        for node in truncate.relations.iter().flatten() {
            let Node::RangeVar(table) = node else {
                return Err(not_yet(node));
            };
            let (name, alias) = self.written_table(Some(table))?;
            let change = Change {
                name,
                alias,
                sets: Vec::new(),
                filter: NONE,
                using: None,
                returning: None,
                delete: true,
                truncate: Some(options),
            };
            statements.push(self.changed_rows(change));
        }
        Ok(statements)
    }

    fn insert_body(&mut self, insert: &InsertStmt) -> Made<Statement> {
        let (name, alias) = self.written_table(insert.relation.as_deref())?;
        let mut parts = Vec::with_capacity(insert.cols.len());
        for node in insert.cols.iter().flatten() {
            let Node::ResTarget(target) = node else {
                return Err(not_yet(node));
            };
            if !target.indirection.is_empty() {
                return clause("InsertIndirection");
            }
            let column = self.intern(target.name.as_deref().unwrap_or_default());
            parts.push((column, Some(self.at(target.location))));
        }
        let columns = self.ast.placed_part_slice(parts);
        // `DEFAULT VALUES` has no query, and the grammar takes no column list with it.
        let source = match &insert.selectStmt {
            None => NONE,
            Some(Node::SelectStmt(select)) => self.query(select)?,
            Some(node) => return Err(not_yet(node)),
        };
        let returning = self.returning(insert.returningClause.as_deref(), name, alias)?;
        let conflict = match insert.onConflictClause.as_deref() {
            Some(conflict) => Some(self.conflict(conflict, name, alias)?),
            None => None,
        };
        let overriding = match insert.r#override {
            OverridingKind::OVERRIDING_USER_VALUE => Overriding::User,
            OverridingKind::OVERRIDING_SYSTEM_VALUE => Overriding::System,
            _ => Overriding::None,
        };
        let index = self.ast.push_insert(Insert {
            name,
            columns,
            source,
            returning,
            conflict,
            copy: false,
            overriding,
            truncate: None,
            filter: NONE,
            values: Slice::default(),
        });
        Ok(Statement::Insert(index))
    }

    /// The name and the alias of the table that a statement writes. `ONLY` is the same as no
    /// `ONLY`, because no table inherits from another. The first part of the name has the place
    /// the name was written at, which is where an error about a missing table points.
    pub(super) fn written_table(&mut self, table: Option<&RangeVar>) -> Made<(Slice, StrRef)> {
        let Some(table) = table else {
            return clause("RangeVar");
        };
        let mut at = Some(self.at(table.location));
        let parts: Vec<(StrRef, Option<Span>)> =
            [&table.catalogname, &table.schemaname, &table.relname]
                .into_iter()
                .filter_map(|part| part.as_deref())
                .map(|part| (self.intern(part), at.take()))
                .collect();
        let name = self.ast.placed_part_slice(parts);
        let alias = match table.alias.as_deref().and_then(|alias| alias.aliasname.as_deref()) {
            Some(alias) => self.intern(alias),
            None => NONE,
        };
        Ok((name, alias))
    }

    /// The from items of `UPDATE ... FROM` or `DELETE ... USING`, or `None` when there are none.
    fn using(&mut self, list: &List) -> Made<Option<Slice>> {
        if list.is_empty() {
            return Ok(None);
        }
        let mut from = Vec::with_capacity(list.len());
        for node in list.iter().flatten() {
            from.push(self.source(node)?);
        }
        Ok(Some(self.ast.source_slice(from)))
    }

    fn changed_rows(&mut self, change: Change) -> Statement {
        let span = self.span;
        self.ast.changed_rows(&mut self.interned, change, span)
    }

    /// The query of a `RETURNING` list, or `None` when the statement has none.
    fn returning(
        &mut self,
        returning: Option<&ReturningClause>,
        name: Slice,
        alias: StrRef,
    ) -> Made<Option<QueryRef>> {
        let Some(returning) = returning else {
            return Ok(None);
        };
        if !returning.options.is_empty() {
            return clause("ReturningOption");
        }
        let targets = self.targets(&returning.exprs)?;
        let span = self.span;
        Ok(Some(self.ast.returning(name, alias, targets, span)))
    }

    /// `ON CONFLICT`. The target is a list of columns. A constraint name, an expression, and a
    /// `WHERE` that picks a partial index are not built yet.
    fn conflict(
        &mut self,
        conflict: &OnConflictClause,
        name: Slice,
        alias: StrRef,
    ) -> Made<Conflict> {
        let mut target = Slice::default();
        if let Some(infer) = conflict.infer.as_deref() {
            if infer.conname.is_some() {
                return clause("OnConflictConstraint");
            }
            if infer.whereClause.is_some() {
                return clause("InferWhere");
            }
            let mut parts = Vec::with_capacity(infer.indexElems.len());
            for node in infer.indexElems.iter().flatten() {
                let Node::IndexElem(element) = node else {
                    return Err(not_yet(node));
                };
                let Some(column) = element.name.as_deref() else {
                    return clause("IndexExpression");
                };
                if !element.collation.is_empty() || !element.opclass.is_empty() {
                    return clause("IndexElemOptions");
                }
                parts.push((self.intern(column), Some(self.at(element.location))));
            }
            target = self.ast.placed_part_slice(parts);
        }
        let action = match conflict.action {
            OnConflictAction::ONCONFLICT_NOTHING => ConflictAction::Nothing,
            OnConflictAction::ONCONFLICT_UPDATE => {
                if conflict.infer.is_none() {
                    return Err(Error::parser(
                        "ON CONFLICT DO UPDATE requires inference specification or constraint name",
                    )
                    .state(SqlState::SYNTAX_ERROR)
                    .hint("For example, ON CONFLICT (column_name).")
                    .with_span(self.at(conflict.location))
                    .into());
                }
                let sets = self.sets(&conflict.targetList)?;
                let condition = self.optional(conflict.whereClause.as_ref())?;
                let span = self.span;
                self.ast.conflict_update(&mut self.interned, name, alias, sets, condition, span)
            }
            _ => return clause("OnConflictSelect"),
        };
        Ok(Conflict { target, action })
    }

    /// The `SET` list of an `UPDATE` or of a `DO UPDATE`, as each column and its new value.
    ///
    /// The grammar gives `(a, b) = source` as one target for each column, each with the same
    /// source and the place of its column in the list.
    fn sets(&mut self, list: &List) -> Made<Vec<Assignment>> {
        let mut sets = Vec::with_capacity(list.len());
        let mut row = Vec::new();
        for node in list.iter().flatten() {
            let Node::ResTarget(target) = node else {
                return Err(not_yet(node));
            };
            if !target.indirection.is_empty() {
                return clause("SetIndirection");
            }
            let column = self.intern(target.name.as_deref().unwrap_or_default());
            let value = match &target.val {
                Some(Node::MultiAssignRef(assign)) => {
                    if assign.colno == 1 {
                        row = self.row_source(assign)?;
                    }
                    let at = usize::try_from(assign.colno - 1).unwrap_or(usize::MAX);
                    match row.get(at) {
                        Some(&value) => value,
                        None => return clause("MultiAssignRef"),
                    }
                }
                Some(node) => self.expr(node)?,
                None => return clause("ResTarget"),
            };
            sets.push(Assignment { column, span: Some(self.at(target.location)), value });
        }
        Ok(sets)
    }

    /// The values of `(a, b) = source`, one for each column. The source is a `ROW` with one value
    /// for each column. A sub-select is not built yet. Any other source is the error of
    /// PostgreSQL.
    fn row_source(&mut self, assign: &MultiAssignRef) -> Made<Vec<ExprRef>> {
        match &assign.source {
            Some(Node::RowExpr(row)) => {
                if i32::try_from(row.args.len()).ok() != Some(assign.ncolumns) {
                    return Err(Error::parser("number of columns does not match number of values")
                        .state(SqlState::SYNTAX_ERROR)
                        .with_span(self.at(row.location))
                        .into());
                }
                let mut values = Vec::with_capacity(row.args.len());
                for arg in row.args.iter().flatten() {
                    values.push(self.expr(arg)?);
                }
                Ok(values)
            }
            Some(Node::SubLink(link)) if link.subLinkType == SubLinkType::EXPR_SUBLINK => {
                clause("MultiExprSubLink")
            }
            source => {
                let location = crate::actions::exprLocation(source.as_ref());
                Err(Error::parser(
                    "source for a multiple-column UPDATE item must be a sub-SELECT or ROW() \
                     expression",
                )
                .state(SqlState::FEATURE_NOT_SUPPORTED)
                .with_span(self.at(location))
                .into())
            }
        }
    }
}
