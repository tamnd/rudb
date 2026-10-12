//! The order that the definitions of a `WITH RECURSIVE` are read in, as `makeDependencyGraph` and
//! `TopologicalSort` of `parse_cte.c` find it.
//!
//! Under `RECURSIVE` a definition can read any definition of its `WITH`, also one after it. A
//! definition reads another when a table of its query without a schema has the other's name and
//! no `WITH` inside the query hides that name. Each definition is read after the ones it reads,
//! so that a forward read finds a definition that is already read. A definition that reads
//! itself is not a dependency, but two that read each other, directly or through others, are
//! refused.

use std::collections::BTreeSet;

use crate::nodes::{CommonTableExpr, NodeType, RangeVar, SelectStmt, Walk, WithClause};

/// The definitions in an order in which each one comes after the ones it reads, or the
/// definition at whose place the error for a mutual recursion points.
///
/// The order is that of `TopologicalSort`: at each position, the first of the definitions left
/// that reads none of the others left goes there, and changes places with the one that was
/// there. When none of them is free, the definition at that position is the one returned.
pub(super) fn ordered(
    ctes: Vec<&CommonTableExpr>,
) -> Result<Vec<&CommonTableExpr>, &CommonTableExpr> {
    let names: Vec<&str> =
        ctes.iter().map(|cte| cte.ctename.as_deref().unwrap_or_default()).collect();
    let mut items: Vec<(usize, BTreeSet<usize>)> = Vec::with_capacity(ctes.len());
    for (at, cte) in ctes.iter().enumerate() {
        let mut graph =
            Graph { names: &names, current: at, reads: BTreeSet::new(), hidden: Vec::new() };
        if let Some(query) = &cte.ctequery {
            query.visit(&mut |node| graph.read(node));
        }
        items.push((at, graph.reads));
    }
    for at in 0..items.len() {
        let Some(free) = (at..items.len()).find(|&other| items[other].1.is_empty()) else {
            return Err(ctes[items[at].0]);
        };
        items.swap(at, free);
        let done = items[at].0;
        for (_, reads) in &mut items[at + 1..] {
            reads.remove(&done);
        }
    }
    Ok(items.into_iter().map(|(at, _)| ctes[at]).collect())
}

/// The walk of `makeDependencyGraphWalker` through the query of one definition.
struct Graph<'n, 'a> {
    /// The names of the definitions of the `WITH`, in the order written.
    names: &'n [&'n str],
    /// The definition whose query is walked.
    current: usize,
    /// The other definitions that it reads.
    reads: BTreeSet<usize>,
    /// The names of the `WITH` clauses inside the query around the node, innermost last, which
    /// hide a definition of the same name.
    hidden: Vec<Vec<&'a str>>,
}

impl<'a> Graph<'_, 'a> {
    fn read(&mut self, node: &'a dyn Walk) {
        if let Some(select) = node.peek::<SelectStmt>()
            && let Some(with) = select.withClause.as_deref()
        {
            self.read_with(select, with);
            return;
        }
        // A `WITH` is walked only from its select, above.
        if node.peek::<WithClause>().is_some() {
            return;
        }
        if let Some(table) = node.peek::<RangeVar>() {
            if table.schemaname.is_none()
                && let Some(name) = table.relname.as_deref()
                && !self.hidden.iter().any(|names| names.contains(&name))
                && let Some(read) = self.names.iter().position(|defined| *defined == name)
                && read != self.current
            {
                self.reads.insert(read);
            }
            return;
        }
        node.walk(&mut |child| self.read(child));
    }

    /// A select with a `WITH`. The names of a `WITH RECURSIVE` hide the definitions of the same
    /// name in all of its definitions, and those of another `WITH` hide them in the definitions
    /// after each one. All of them hide them in the rest of the select.
    fn read_with(&mut self, select: &'a SelectStmt, with: &'a WithClause) {
        let ctes: Vec<&CommonTableExpr> =
            with.ctes.iter().flatten().filter_map(CommonTableExpr::peek).collect();
        if with.recursive {
            self.hidden
                .push(ctes.iter().map(|cte| cte.ctename.as_deref().unwrap_or_default()).collect());
            for cte in ctes {
                if let Some(query) = &cte.ctequery {
                    query.visit(&mut |node| self.read(node));
                }
            }
        } else {
            self.hidden.push(Vec::new());
            for cte in ctes {
                if let Some(query) = &cte.ctequery {
                    query.visit(&mut |node| self.read(node));
                }
                if let Some(names) = self.hidden.last_mut() {
                    names.push(cte.ctename.as_deref().unwrap_or_default());
                }
            }
        }
        select.walk(&mut |child| self.read(child));
        self.hidden.pop();
    }
}
