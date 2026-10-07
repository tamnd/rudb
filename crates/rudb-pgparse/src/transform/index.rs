//! `CREATE INDEX`.

use rudb_parse::NONE;
use rudb_parse::ast::{Expr, Index, Slice, Statement};

use super::{Made, Transform, clause, not_yet};
use crate::nodes::{IndexStmt, Node};

impl Transform<'_> {
    /// `CREATE INDEX`. An index with no name has an empty name here, and the binder names it as
    /// PostgreSQL does.
    ///
    /// The grammar writes `btree` when the statement names no method, so `btree` is the same as
    /// no method. The order of an element is dropped, as the pin drops it. A column element keeps
    /// the place of its name, so that an error about the column points at it.
    pub(super) fn create_index(&mut self, index: &IndexStmt) -> Made<Statement> {
        if index.whereClause.is_some() {
            return clause("IndexWhere");
        }
        if !index.indexIncludingParams.is_empty() {
            return clause("IndexInclude");
        }
        if !index.options.is_empty() || index.tableSpace.is_some() {
            return clause("IndexOptions");
        }
        if index.concurrent || index.nulls_not_distinct {
            return clause("IndexStmt");
        }
        let (table, _) = self.written_table(index.relation.as_deref())?;
        let name = match index.idxname.as_deref() {
            Some(name) => {
                let name = self.intern(name);
                self.ast.part_slice(vec![name])
            }
            None => Slice::default(),
        };
        let using = match index.accessMethod.as_deref() {
            None | Some("btree") => NONE,
            Some(method) => self.intern(method),
        };
        let mut elements = Vec::with_capacity(index.indexParams.len());
        for node in index.indexParams.iter().flatten() {
            let Node::IndexElem(element) = node else {
                return Err(not_yet(node));
            };
            if !element.collation.is_empty()
                || !element.opclass.is_empty()
                || !element.opclassopts.is_empty()
            {
                return clause("IndexElemOptions");
            }
            let expr = match (element.name.as_deref(), &element.expr) {
                (Some(column), _) => {
                    let column = self.intern(column);
                    let at = self.at(element.location);
                    let name = self.ast.placed_part_slice([(column, Some(at))]);
                    self.push(Expr::Column { name }, element.location)
                }
                (None, Some(expr)) => self.expr(expr)?,
                (None, None) => return clause("IndexElem"),
            };
            elements.push(expr);
        }
        let index = Index {
            name,
            table,
            drop: false,
            quiet: index.if_not_exists,
            unique: index.unique,
            or_replace: false,
            using,
            elements: self.ast.expr_slice(elements),
        };
        Ok(Statement::Index(self.ast.push_index(index)))
    }
}
