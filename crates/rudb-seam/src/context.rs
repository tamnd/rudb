//! What the planner tells a strategy about the situation it is being asked into.

use rudb_common::LogicalType;

use crate::seam::SeamId;
use crate::settings::Settings;

/// The situation at one seam, at plan time.
///
/// Deliberately small and deliberately static. This is what the planner knows, not what the
/// runtime discovers, because a choice that depends on what the runtime discovers is a choice that
/// cannot be printed by `EXPLAIN` before the query runs, and a plan nobody can read before running
/// it is a plan nobody can argue with.
///
/// Fields get added as milestones need them. `forms`, which says what the input columns can arrive
/// as without being decoded, is F1 and is the field that makes a layout decision reach an operator
/// choice instead of being erased at the scan.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    seam: SeamId,
    settings: &'a Settings,
    types: &'a [LogicalType],
    estimated_rows: Option<u64>,
    estimated_distinct: Option<u64>,
    memory_budget: u64,
    thread_count: usize,
}

impl<'a> Context<'a> {
    /// The least a caller can say: which seam, and what the session is set to.
    ///
    /// Everything else has a neutral answer, so a call site that genuinely knows nothing about
    /// cardinality does not have to invent a number in order to compile.
    #[must_use]
    pub fn new(seam: SeamId, settings: &'a Settings) -> Self {
        Self {
            seam,
            settings,
            types: &[],
            estimated_rows: None,
            estimated_distinct: None,
            memory_budget: u64::MAX,
            thread_count: 1,
        }
    }

    /// The types of the columns the seam is being asked about.
    #[must_use]
    pub fn with_types(mut self, types: &'a [LogicalType]) -> Self {
        self.types = types;
        self
    }

    /// How many rows the optimizer thinks will arrive.
    #[must_use]
    pub fn with_estimated_rows(mut self, rows: u64) -> Self {
        self.estimated_rows = Some(rows);
        self
    }

    /// How many distinct values the optimizer thinks will arrive.
    #[must_use]
    pub fn with_estimated_distinct(mut self, distinct: u64) -> Self {
        self.estimated_distinct = Some(distinct);
        self
    }

    /// How many bytes this part of the query is allowed.
    #[must_use]
    pub fn with_memory_budget(mut self, bytes: u64) -> Self {
        self.memory_budget = bytes;
        self
    }

    /// How many threads will run it.
    #[must_use]
    pub fn with_thread_count(mut self, threads: usize) -> Self {
        self.thread_count = threads;
        self
    }

    /// Which seam is being chosen at.
    #[must_use]
    pub fn seam(&self) -> SeamId {
        self.seam
    }

    /// The session settings, which is where a pin comes from.
    #[must_use]
    pub fn settings(&self) -> &'a Settings {
        self.settings
    }

    /// The column types, empty when the caller had none to give.
    #[must_use]
    pub fn types(&self) -> &'a [LogicalType] {
        self.types
    }

    /// The row estimate, or `None` when nothing estimated it.
    ///
    /// `None` and zero are different answers and a strategy that treats them the same will pick
    /// the small input path for a table nobody has looked at yet.
    #[must_use]
    pub fn estimated_rows(&self) -> Option<u64> {
        self.estimated_rows
    }

    /// The distinct count estimate, or `None` when nothing estimated it.
    #[must_use]
    pub fn estimated_distinct(&self) -> Option<u64> {
        self.estimated_distinct
    }

    /// The byte budget, which is `u64::MAX` when the caller did not set one.
    #[must_use]
    pub fn memory_budget(&self) -> u64 {
        self.memory_budget
    }

    /// The thread count, which is one until F4.
    #[must_use]
    pub fn thread_count(&self) -> usize {
        self.thread_count
    }
}
