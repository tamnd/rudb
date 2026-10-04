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

use std::sync::{Arc, Mutex};

use rudb_common::{Error, Reservation, Result, Value};
use rudb_metrics::Report;
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::build::{Round, build_round};
use crate::gather::{self, Gather, Gathering};
use crate::key::{Key, RowSet};
use crate::rows;
use crate::schema::Schema;

/// The sink at the end of the anchor, whose finish runs the rounds.
#[derive(Debug)]
pub(crate) struct Fixpoint<'a> {
    round: Round<'a>,
    schema: Schema,
    all: bool,
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
    pub(crate) fn new(round: Round<'a>, schema: Schema, all: bool) -> (Self, Buffered) {
        let out = Buffered::new();
        let held = Mutex::new(round.memory.reservation());
        let fixpoint = Self {
            round,
            schema,
            all,
            anchor: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held,
            out: out.clone(),
        };
        (fixpoint, out)
    }

    /// The rows one round adds, given the rows the round before added.
    fn round(&self, working: &[Vec<Value>], lease: &Lease<'_>) -> Result<Vec<Vec<Value>>> {
        let mut charged = self.round.memory.reservation();
        let table = Buffered::new();
        table.fill(rows::chunks(&self.schema.types(), working, &mut charged)?)?;
        let mut held = self.round.outer.clone();
        held.push((self.round.cte, table));
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
        let mut seen = RowSet::default();
        let mut working = std::mem::take(&mut *self.anchor.lock().map_err(poisoned)?);
        if !self.all {
            working.retain(|row| seen.insert(Key(row.clone())));
        }
        let mut out = working.clone();
        while !working.is_empty() {
            self.round.cancel.check()?;
            let mut made = self.round(&working, threads)?;
            if !self.all {
                made.retain(|row| seen.insert(Key(row.clone())));
            }
            out.extend(made.iter().cloned());
            working = made;
        }
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.schema.types(), &out, &mut held)?;
        self.out.fill(chunks)?;
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows of a recursive definition")
}
