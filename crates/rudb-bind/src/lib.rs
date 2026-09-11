//! Name, type and overload resolution, subquery binding, and the bound logical plan.
//!
//! Rank 10 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! The binder is the pass that turns what someone wrote into what it means. A parse tree says
//! `SELECT x FROM t`, and only the binder can say which table `t` is, which column `x` is, what
//! type it has, and therefore what the query does. Everything after this point works on the
//! answer rather than on the question: the optimizer never resolves a name and the executor never
//! decides a type.
//!
//! There are two entry points and the difference between them is what they can return. [`bind`]
//! takes a statement that produces rows and gives back a [`rudb_plan::Plan`], which is what an
//! optimizer and an executor want. [`bind_statement`] takes any statement and gives back a
//! [`Bound`], which is a plan for a query and a resolved catalog operation for `CREATE TABLE`,
//! `DROP TABLE` and `INSERT`. DDL is not a plan node, for the reason `statement.rs` gives.
//!
//! What it does not do yet is subqueries, window functions, `WITH`, and every statement outside
//! those four. Each of those is an error naming what was written rather than a silently wrong
//! plan, which is the rule the whole front end follows.

#![forbid(unsafe_code)]

mod binder;
mod expr;
mod file;
mod scope;
mod statement;

pub use binder::{bind, bind_sql};
pub use statement::{Bound, CreateTable, DropTable, Insert, bind_statement, bind_statement_sql};

#[cfg(test)]
mod tests;
