//! Plans a prepared statement keeps, so that an execution with other values sets them into a plan
//! bound before rather than binding the statement again.
//!
//! The binder turns a parameter into a constant holding its value, and with [`Parameters::keep_in`]
//! it also notes which constant is which parameter's (see [`Plan::watch_parameters`]). A plan whose
//! binding never read one of those constants did nothing with the values but carry them, so the
//! same plan with other values of the same types in the same constants is the plan binding those
//! values would have built. A binding that did read one, a `LIMIT $1` worked out to a row count or
//! a call folded at bind time, made something of the value that a new value set into the constant
//! would not reach, and the plan is not kept.
//!
//! What else binding reads is the catalog's names, the settings, and the facts the stored tables
//! keep about their rows. The first two are the key a plan is kept under, as the caller's numbers
//! for them. The facts move with every write, so a plan taken out again has them read afresh off
//! the catalog in hand, table by table, as the binder would have. A plan that read anything else,
//! a file, a table function or the clock, is not kept.

use std::sync::{Arc, Mutex, PoisonError};

use rudb_catalog::{Catalog, MIRROR_CATALOG, QualifiedName};
use rudb_common::LogicalType;
use rudb_plan::{Node, Plan, Watched};

use crate::Parameters;
use crate::binder::attach_facts;

/// How many plans one statement keeps, one for each set of parameter types it was run with.
///
/// A driver runs a statement with one set of types almost always, and one that alternates two
/// integer widths for the same key gets two plans rather than a bind on every other call.
const HELD: usize = 4;

/// The plans one prepared statement keeps, shared by every execution of it.
#[derive(Debug, Clone, Default)]
pub struct Kept(Arc<Mutex<Held>>);

impl PartialEq for Kept {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug, Default)]
struct Held {
    plans: Vec<Entry>,
    /// Counts takings, so that the plan taken least recently is the one a new one replaces.
    clock: u64,
}

#[derive(Debug)]
struct Entry {
    /// The caller's numbers for the catalog's names and the settings the plan was bound under.
    at: (u64, u64),
    /// The type of each value, in the order they were given.
    types: Vec<LogicalType>,
    plan: Plan,
    watched: Watched,
    used: u64,
}

impl Kept {
    /// A plan bound before under `at` with values of the types of `parameters`, holding their
    /// values and the facts `catalog` has now, or `None` when there is none to take.
    #[must_use]
    pub fn take(&self, at: (u64, u64), parameters: &Parameters, catalog: &Catalog) -> Option<Plan> {
        if !parameters.plain() {
            return None;
        }
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.clock += 1;
        let clock = held.clock;
        let entry = held.plans.iter_mut().find(|entry| {
            entry.at == at
                && entry.types.len() == parameters.len()
                && parameters
                    .values()
                    .zip(&entry.types)
                    .all(|((_, value), ty)| value.logical_type() == *ty)
        })?;
        entry.used = clock;
        let mut plan = entry.plan.clone();
        for (slot, name) in &entry.watched.slots {
            plan.set_value(*slot, parameters.get(name)?.clone());
        }
        for (index, [catalog_name, schema, table], ascending) in &entry.watched.tables {
            let name = QualifiedName::new(catalog_name, schema, table);
            let table = catalog.table(&name).ok()?;
            plan.forget_facts(*index);
            attach_facts(&mut plan, *index, table, *ascending);
        }
        Some(plan)
    }

    /// Keeps `plan`, just bound under `at` with `parameters`, when it can be run again with other
    /// values. Stops the plan watching its parameters either way.
    pub fn keep(&self, at: (u64, u64), parameters: &Parameters, plan: &mut Plan) {
        let Some(watched) = plan.take_parameters() else { return };
        if !parameters.plain() || !rebindable(plan, &watched) {
            return;
        }
        let types = parameters.values().map(|(_, value)| value.logical_type()).collect();
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.clock += 1;
        let used = held.clock;
        // A plan bound under other names or settings is not taken again once these are the ones.
        held.plans.retain(|entry| entry.at == at);
        if held.plans.len() >= HELD
            && let Some(oldest) = (0..held.plans.len()).min_by_key(|&at| held.plans[at].used)
        {
            held.plans.swap_remove(oldest);
        }
        held.plans.push(Entry { at, types, plan: plan.clone(), watched, used });
    }

    /// How many plans are kept, which is what a test asks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).plans.len()
    }

    /// Whether no plan is kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whether everything `plan` reads is a stored table whose facts `watched` can read again, or no
/// table at all.
///
/// A table function reads its arguments at bind time and a file its footer, neither of which the
/// catalog's numbers say anything about, and a native mirror of a file can be built again under
/// the same names. The fetches are a rewrite's and are never bound, but are refused rather than
/// assumed so.
fn rebindable(plan: &Plan, watched: &Watched) -> bool {
    plan.measured_count() == 0
        && plan.wanted_mirrors().is_empty()
        && watched.tables.iter().all(|(_, [catalog, ..], _)| catalog != MIRROR_CATALOG)
        && (0..plan.node_count()).all(|at| {
            let at = u32::try_from(at).expect("a plan has fewer than 2^32 nodes");
            !matches!(
                plan.node(at),
                Node::TableFunction { .. }
                    | Node::LateralFunction { .. }
                    | Node::Fetch { .. }
                    | Node::TableFetch { .. }
                    | Node::Consistent { .. }
            )
        })
}
