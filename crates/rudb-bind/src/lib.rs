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
//! What it does not do yet is subqueries, window functions, `WITH`, `VALUES` as a query, and every
//! statement that is not a `SELECT`. Each of those is an error naming what was written rather than
//! a silently wrong plan, which is the rule the whole front end follows.

#![forbid(unsafe_code)]

mod binder;
mod expr;
mod scope;

pub use binder::{bind, bind_sql};

#[cfg(test)]
mod tests;
