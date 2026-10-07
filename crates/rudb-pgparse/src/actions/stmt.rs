//! The actions of the statement lists, `parse_toplevel` and `stmtmulti`, and of the small
//! statements that have no file of their own: `WAIT FOR`.

use super::{Parser, makeRawStmt, updateRawStmtEnd};
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

impl rules::parse_toplevel for Parser<'_> {
    fn parse_toplevel_1(&mut self, v1: List) -> Result<List, Error> {
        self.parsetree = v1;
        Ok(List::new())
    }

    fn parse_toplevel_2(&mut self, v2: Option<Box<TypeName>>) -> Result<List, Error> {
        self.parsetree = vec![v2.map(Node::TypeName)];
        Ok(List::new())
    }

    fn parse_toplevel_3(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(v2, at2).into())];
        Ok(List::new())
    }

    fn parse_toplevel_4(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(assign(v2, 1), at2).into())];
        Ok(List::new())
    }

    fn parse_toplevel_5(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(assign(v2, 2), at2).into())];
        Ok(List::new())
    }

    fn parse_toplevel_6(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(assign(v2, 3), at2).into())];
        Ok(List::new())
    }
}

/// Sets `nnames` of the `PLAssignStmt` of the `MODE_PLPGSQL_ASSIGN` modes.
fn assign(stmt: Option<Node>, nnames: i32) -> Option<Node> {
    let mut stmt = stmt;
    if let Some(n) = stmt.as_mut().and_then(PLAssignStmt::peek_mut) {
        n.nnames = nnames;
    }
    stmt
}

impl rules::stmtmulti for Parser<'_> {
    fn stmtmulti_1(
        &mut self,
        v1: List,
        v3: Option<Node>,
        at2: i32,
        at3: i32,
    ) -> Result<List, Error> {
        let mut list = v1;
        // The length of the statement before the `;`.
        if let Some(rs) = list.last_mut().and_then(Option::as_mut).and_then(RawStmt::peek_mut) {
            updateRawStmtEnd(rs, at2);
        }
        if v3.is_some() {
            list.push(Some(makeRawStmt(v3, at3).into()));
        }
        Ok(list)
    }

    fn stmtmulti_2(&mut self, v1: Option<Node>, at1: i32) -> Result<List, Error> {
        if v1.is_some() { Ok(vec![Some(makeRawStmt(v1, at1).into())]) } else { Ok(List::new()) }
    }
}

impl rules::WaitStmt for Parser<'_> {
    fn WaitStmt_1(&mut self, v4: Option<Str>, v5: List, at4: i32) -> Result<Option<Node>, Error> {
        Ok(Some(WaitStmt { lsn_literal: v4, options: v5, lsn_location: at4 }.into()))
    }
}
