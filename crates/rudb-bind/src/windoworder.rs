//! The order PostgreSQL computes the windows of one query in.
//!
//! Each window sorts the rows the window below it made, so the sort of the last one is the order
//! of a query with no `ORDER BY`, and the sorts before it decide the order of the rows that tie
//! in it. PostgreSQL puts the windows in order in `select_active_windows` in `planner.c`. It
//! makes a list of the partition keys of each window followed by the order keys that are not
//! partition keys, and sorts the windows by these lists with `common_prefix_cmp`, which compares
//! two lists one key at a time:
//!
//! - the key with the higher sort group number goes first,
//! - then a descending key goes before an ascending one, because the `>` operator of a type has
//!   a higher number than its `<`,
//! - then a key with the nulls first goes before a key with the nulls last,
//! - and when one list is the start of the other, the longer list goes first.
//!
//! An expression gets its sort group number the first time a clause sorts or groups on it, and
//! keeps it. The clauses are numbered in this order: the `ORDER BY` of the query, its
//! `GROUP BY`, the targets of a `DISTINCT`, and then the windows. The windows of the `WINDOW`
//! clause come before the windows written in an `OVER`, and in each window the order keys come
//! before the partition keys, which is the order `transformWindowDefinitions` reads them in.
//!
//! PostgreSQL sorts the windows with its quicksort, which is not stable, and the sort here is
//! stable. That makes no difference to the rows: two windows that compare equal sort on the same
//! keys, so whichever of the two is second finds the rows already sorted.
//!
//! A window of the `WINDOW` clause that no call uses also numbers its keys in PostgreSQL. Its keys
//! are not bound here, so they get no number.

use std::cmp::Ordering;

use rudb_parse::ast::Distinct;
use rudb_plan::ExprRef;

use crate::binder::{Binder, WindowRun};

/// One key of a window, as `common_prefix_cmp` compares it.
#[derive(Debug, Clone, Copy)]
struct Key {
    /// The sort group number of the expression.
    number: usize,
    descending: bool,
    nulls_first: bool,
}

impl Binder<'_> {
    /// The expressions of a query that PostgreSQL numbers before the keys of its windows, in the
    /// order it numbers them: what the `ORDER BY` sorts on, the groups, and the targets of a
    /// `DISTINCT`.
    pub(crate) fn numbered_before_windows(
        &mut self,
        sorted: impl IntoIterator<Item = ExprRef>,
        distinct: &Distinct,
        targets: &[ExprRef],
    ) -> Vec<ExprRef> {
        let mut numbered: Vec<ExprRef> = sorted.into_iter().collect();
        if let Some(aggregation) = &self.aggregation {
            let (index, groups) = (aggregation.index, aggregation.groups.clone());
            for (at, group) in groups.into_iter().enumerate() {
                let ty = self.plan().expr_type(group).clone();
                numbered.push(self.column(index, at, ty));
            }
        }
        if *distinct == Distinct::Yes {
            numbered.extend_from_slice(targets);
        }
        numbered
    }

    /// The runs in the order PostgreSQL computes them in, given the expressions it numbers before
    /// their keys.
    pub(crate) fn postgres_window_order(
        &self,
        runs: Vec<WindowRun>,
        numbered: &[ExprRef],
    ) -> Vec<WindowRun> {
        let mut groups: Vec<ExprRef> = Vec::new();
        let mut number =
            |expr: ExprRef| match groups.iter().position(|&held| self.same_expr(held, expr)) {
                Some(at) => at,
                None => {
                    groups.push(expr);
                    groups.len() - 1
                }
            };
        for &expr in numbered {
            number(expr);
        }
        // The windows of the `WINDOW` clause in the order it wrote them, and then the others in
        // the order their first call was written in, which is the order the runs were opened in.
        let mut written: Vec<usize> = (0..runs.len()).collect();
        written.sort_by_key(|&at| {
            let run = &runs[at];
            if run.named { (0, run.spec as usize) } else { (1, at) }
        });
        for &at in &written {
            for key in &runs[at].order {
                number(key.expr);
            }
            for &expr in &runs[at].partition {
                number(expr);
            }
        }

        let lists: Vec<Vec<Key>> = runs
            .iter()
            .map(|run| {
                // A partition key that the window also orders on takes the direction of the order
                // key, and the order key is then left out of the list.
                let mut keys: Vec<Key> = Vec::new();
                for &expr in &run.partition {
                    let key = match run.order.iter().find(|key| self.same_expr(key.expr, expr)) {
                        Some(key) => Key {
                            number: number(expr),
                            descending: key.descending,
                            nulls_first: key.nulls_first,
                        },
                        None => Key { number: number(expr), descending: false, nulls_first: false },
                    };
                    if !keys.iter().any(|held| held.number == key.number) {
                        keys.push(key);
                    }
                }
                for key in &run.order {
                    let number = number(key.expr);
                    if !keys.iter().any(|held| held.number == number) {
                        let (descending, nulls_first) = (key.descending, key.nulls_first);
                        keys.push(Key { number, descending, nulls_first });
                    }
                }
                keys
            })
            .collect();

        let mut order: Vec<usize> = (0..runs.len()).collect();
        order.sort_by(|&left, &right| common_prefix(&lists[left], &lists[right]));
        let mut runs: Vec<Option<WindowRun>> = runs.into_iter().map(Some).collect();
        order.into_iter().filter_map(|at| runs[at].take()).collect()
    }
}

/// `common_prefix_cmp`: `Less` when the window with the keys `left` is computed first.
fn common_prefix(left: &[Key], right: &[Key]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let ordering = right
            .number
            .cmp(&left.number)
            .then(right.descending.cmp(&left.descending))
            .then(right.nulls_first.cmp(&left.nulls_first));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    right.len().cmp(&left.len())
}
