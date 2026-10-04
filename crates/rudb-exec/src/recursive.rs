//! `WITH RECURSIVE`, run to a fixpoint.
//!
//! The anchor is an ordinary pipeline ending in [`Fixpoint`], which gathers its rows. Everything
//! else happens in the finish. Each round builds the recursive side as a query of its own, with the
//! rows the round before added standing in for the definition's name, runs it on the threads this
//! finish already holds, and keeps what it produced. The rounds stop when one adds nothing.
//!
//! A query of its own per round rather than one set of pipelines run again, because an operator
//! here finishes once: a sort sorts what it was given and a hash table is built once. Building again
//! is cheap next to running, since what is built is a handful of operators over a plan that is
//! already optimized.
//!
//! Without `ALL` a row that any round already produced is not produced again, and that is also what
//! ends a walk over a graph with a cycle in it. With `ALL` nothing is checked and a cycle runs until
//! the query is cancelled or out of memory, which is what the pinned build does.
//!
//! With `USING KEY` the rows produced are a table keyed on the key columns rather than a list. A
//! row whose key is there already replaces the row that had it, in the order the rows came, so the
//! last one wins within a round. With `ALL` the next round reads every row this round made, and
//! without it only one row per key whose value this round changed, a key that came back with the
//! value it had being no work at all. A `recurring.` read sees the table as it stood when the round
//! began, and without a key it sees every row produced so far.

use std::sync::{Arc, Mutex};

use rudb_common::{Error, Reservation, Result, Value};
use rudb_metrics::Report;
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::build::{Round, build_round};
use crate::gather::{self, Gather, Gathering};
use crate::key::{Key, RowMap, RowSet, same};
use crate::rows;
use crate::schema::Schema;

/// The sink at the end of the anchor, whose finish runs the rounds.
#[derive(Debug)]
pub(crate) struct Fixpoint<'a> {
    round: Round<'a>,
    schema: Schema,
    all: bool,
    /// The positions of the `USING KEY` columns, empty without one.
    key: Vec<usize>,
    /// The anchor's rows, as every instance gathered them.
    anchor: Mutex<Vec<Vec<Value>>>,
    /// What the anchor's rows are charged, given back once the finished chunks are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the finished chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

impl<'a> Fixpoint<'a> {
    /// The sink for the anchor, and the source every row of every round comes out of.
    pub(crate) fn new(
        round: Round<'a>,
        schema: Schema,
        all: bool,
        key: Vec<usize>,
    ) -> (Self, Buffered) {
        let out = Buffered::new();
        let held = Mutex::new(round.memory.reservation());
        let fixpoint = Self {
            round,
            schema,
            all,
            key,
            anchor: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held,
            out: out.clone(),
        };
        (fixpoint, out)
    }

    /// The rows one round makes, given the rows the round before passed on and every row produced
    /// so far.
    fn round(
        &self,
        working: &[Vec<Value>],
        so_far: &[Vec<Value>],
        lease: &Lease<'_>,
    ) -> Result<Vec<Vec<Value>>> {
        let types = self.schema.types();
        let mut charged = self.round.memory.reservation();
        let table = Buffered::new();
        table.fill(rows::chunks(&types, working, &mut charged)?)?;
        let mut held = self.round.outer.clone();
        held.push((self.round.cte, table));
        if let Some(recurring) = self.round.recurring {
            let snapshot = Buffered::new();
            snapshot.fill(rows::chunks(&types, so_far, &mut charged)?)?;
            held.push((recurring, snapshot));
        }
        let (gather, made) = Gather::new(&self.round.memory);
        let report = Report::new();
        let query = build_round(&self.round, &report, held, Arc::new(gather))?;
        query.run_on(&self.round.cancel, lease)?;
        made.take()
    }
}

impl Sink for Fixpoint<'_> {
    type Local = Gathering;

    fn local(&self) -> Gathering {
        gather::gathering(&self.round.memory)
    }

    /// Not yet, because the anchor's rows come out first and in the order they arrived.
    fn parallel(&self) -> bool {
        false
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathering) -> Result<Progress> {
        gather::take(chunk, local)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathering) -> Result<()> {
        let (rows, charged) = gather::into_parts(local);
        self.anchor.lock().map_err(poisoned)?.extend(rows);
        self.charged.lock().map_err(poisoned)?.push(charged);
        Ok(())
    }

    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        let anchor = std::mem::take(&mut *self.anchor.lock().map_err(poisoned)?);
        let out = if self.key.is_empty() {
            self.listed(anchor, threads)?
        } else {
            self.keyed(anchor, threads)?
        };
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.schema.types(), &out, &mut held)?;
        self.out.fill(chunks)?;
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

impl Fixpoint<'_> {
    /// Every row every round produced, in the order they came, without a key.
    fn listed(&self, mut working: Vec<Vec<Value>>, threads: &Lease<'_>) -> Result<Vec<Vec<Value>>> {
        let mut seen = RowSet::default();
        if !self.all {
            working.retain(|row| seen.insert(Key(row.clone())));
        }
        let mut out = working.clone();
        while !working.is_empty() {
            self.round.cancel.check()?;
            let mut made = self.round(&working, &out, threads)?;
            if !self.all {
                made.retain(|row| seen.insert(Key(row.clone())));
            }
            out.extend(made.iter().cloned());
            working = made;
        }
        Ok(out)
    }

    /// The last row each key was given, keys in the order they first came, with a key.
    fn keyed(&self, anchor: Vec<Vec<Value>>, threads: &Lease<'_>) -> Result<Vec<Vec<Value>>> {
        let mut table = Keyed::default();
        let mut working = self.apply(&mut table, anchor);
        if self.round.once {
            let made = self.round(&working, &table.rows, threads)?;
            self.apply(&mut table, made);
            return Ok(table.rows);
        }
        while !working.is_empty() {
            self.round.cancel.check()?;
            let made = self.round(&working, &table.rows, threads)?;
            working = self.apply(&mut table, made);
        }
        Ok(table.rows)
    }

    /// Puts what a round made into the table and answers what the next round reads.
    fn apply(&self, table: &mut Keyed, made: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        if self.all {
            for row in &made {
                table.put(self.key_of(row), row.clone());
            }
            return made;
        }
        // What each key this round touched held before it, in the order the keys came.
        let mut touched: Vec<(usize, Option<Vec<Value>>)> = Vec::new();
        let mut first = vec![false; table.rows.len()];
        for row in made {
            let key = self.key_of(&row);
            match table.at.get(&key) {
                Some(&slot) => {
                    if slot < first.len() && !first[slot] {
                        first[slot] = true;
                        touched.push((slot, Some(std::mem::replace(&mut table.rows[slot], row))));
                    } else {
                        table.rows[slot] = row;
                    }
                }
                None => {
                    touched.push((table.rows.len(), None));
                    table.put(key, row);
                }
            }
        }
        touched
            .into_iter()
            .filter(|(slot, before)| {
                before.as_ref().is_none_or(|before| {
                    !before.iter().zip(&table.rows[*slot]).all(|(was, now)| same(was, now))
                })
            })
            .map(|(slot, _)| table.rows[slot].clone())
            .collect()
    }

    /// The values of a row's key columns, as one key.
    fn key_of(&self, row: &[Value]) -> Key {
        Key(self.key.iter().map(|&at| row[at].clone()).collect())
    }
}

/// The rows of a keyed recursion, one per key, and where each key's row is.
#[derive(Default)]
struct Keyed {
    rows: Vec<Vec<Value>>,
    at: RowMap<usize>,
}

impl Keyed {
    /// Gives `key` the row, replacing the one it had or adding it after every other.
    fn put(&mut self, key: Key, row: Vec<Value>) {
        match self.at.get(&key) {
            Some(&slot) => self.rows[slot] = row,
            None => {
                self.at.insert(key, self.rows.len());
                self.rows.push(row);
            }
        }
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows of a recursive definition")
}
