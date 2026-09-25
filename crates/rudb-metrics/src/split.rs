//! A query's time split into the kinds of work an engine is built out of.
//!
//! The operator rows say which operator held the time and the stages say which phase inside it. A
//! person deciding what to build next wants a third answer that neither gives on its own: of the
//! time this query took, how much was reading and decoding, how much was deciding which rows to
//! keep, building hash tables, probing them, folding groups, working on strings and copying rows
//! into new chunks. [`Split`] is that answer, worked out from the rows and the stages rather than
//! measured again, so it costs nothing that the document did not already cost.
//!
//! Each operator's wall time goes to exactly one place. The stages that name a kind of work of
//! their own, which are [`Stage::Filter`], [`Stage::Build`], [`Stage::Strings`] and
//! [`Stage::Materialize`], are taken out of the operator that charged them and go to that kind.
//! What is left goes to the kind the operator is. A stage inside another is charged once, which is
//! [`Timing`](rudb_common::stage::Timing)'s rule, so the parts add up to the operators' own time
//! and never to more.

use rudb_common::stage::Stage;

use crate::document::{Document, Operator};

/// Wall time by kind of work, summed over every operator and every instance of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Split {
    /// Reading a table or a file and decoding it, less any filter the scan ran for itself.
    pub scan_ns: u64,
    /// Deciding which rows a predicate keeps, in a filter or inside a scan.
    pub filter_ns: u64,
    /// Keeping a join's gathered side and building the table over it.
    pub build_ns: u64,
    /// Looking up the driving rows in a join's table, less the copies of the matched rows.
    pub probe_ns: u64,
    /// Grouped and ungrouped aggregates and `DISTINCT`.
    pub aggregate_ns: u64,
    /// Expression steps that read or write strings, wherever they ran.
    pub strings_ns: u64,
    /// Copying rows into new chunks: a join's matched columns, a late fetch of columns, and the
    /// answer flattened for the caller.
    pub materialize_ns: u64,
    /// Everything else, such as projections of numbers, sorts and limits.
    pub other_ns: u64,
}

/// Which part of a [`Split`] the rest of an operator's time goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Scan,
    Filter,
    Build,
    Probe,
    Aggregate,
    Materialize,
    Other,
}

/// The part an operator's kind is, going by the names the executor gives its rows.
fn part(kind: &str) -> Part {
    match kind {
        "Scan" | "FileScan" | "CteScan" => Part::Scan,
        "Filter" => Part::Filter,
        "Gather" | "Keep" => Part::Build,
        "Probe" | "Join" | "LinkJoin" | "Mark" | "Pad" | "Broadcast" | "CrossProduct" => {
            Part::Probe
        }
        "Aggregate" | "HashAggregate" | "Distinct" => Part::Aggregate,
        "Fetch" | "TableFetch" => Part::Materialize,
        _ => Part::Other,
    }
}

impl Split {
    /// The parts with their names, in the order the work usually goes through them.
    #[must_use]
    pub const fn parts(&self) -> [(&'static str, u64); 8] {
        [
            ("scan", self.scan_ns),
            ("filter", self.filter_ns),
            ("build", self.build_ns),
            ("probe", self.probe_ns),
            ("aggregate", self.aggregate_ns),
            ("strings", self.strings_ns),
            ("materialize", self.materialize_ns),
            ("other", self.other_ns),
        ]
    }

    /// Every part, added up.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.parts().iter().fold(0, |sum, (_, nanos)| sum.saturating_add(*nanos))
    }

    /// Adds one operator's row.
    fn add(&mut self, operator: &Operator) {
        let stages = &operator.stages;
        let filter = stages.nanos(Stage::Filter);
        let build = stages.nanos(Stage::Build);
        let strings = stages.nanos(Stage::Strings);
        let materialize = stages.nanos(Stage::Materialize);
        let carved =
            filter.saturating_add(build).saturating_add(strings).saturating_add(materialize);
        let own = operator.wall_ns.saturating_sub(carved);
        self.filter_ns = self.filter_ns.saturating_add(filter);
        self.build_ns = self.build_ns.saturating_add(build);
        self.strings_ns = self.strings_ns.saturating_add(strings);
        self.materialize_ns = self.materialize_ns.saturating_add(materialize);
        let slot = match part(&operator.kind) {
            Part::Scan => &mut self.scan_ns,
            Part::Filter => &mut self.filter_ns,
            Part::Build => &mut self.build_ns,
            Part::Probe => &mut self.probe_ns,
            Part::Aggregate => &mut self.aggregate_ns,
            Part::Materialize => &mut self.materialize_ns,
            Part::Other => &mut self.other_ns,
        };
        *slot = slot.saturating_add(own);
    }
}

impl Document {
    /// The query's time by kind of work. See [`Split`].
    ///
    /// Worked out from the operator rows each time it is asked for rather than stored, so it can
    /// never disagree with them.
    #[must_use]
    pub fn split(&self) -> Split {
        let mut split = Split::default();
        for operator in &self.operators {
            split.add(operator);
        }
        split.materialize_ns = split.materialize_ns.saturating_add(self.timing.result_ns);
        split
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::stage::{Spent, Stage};

    use super::Split;
    use crate::document::{Document, Operator};

    fn operator(kind: &str, wall_ns: u64, stages: Spent) -> Operator {
        let mut operator = Operator::new(0, 0, kind);
        operator.wall_ns = wall_ns;
        operator.stages = stages;
        operator
    }

    #[test]
    fn a_scan_that_filtered_for_itself_gives_the_filter_back() {
        let mut document = Document::new("select 1");
        let mut stages = Spent::of(Stage::Filter, 300, 0);
        stages.add(Spent::of(Stage::Decode, 200, 0));
        document.operators.push(operator("Scan", 1000, stages));
        let split = document.split();
        assert_eq!(split.filter_ns, 300);
        assert_eq!(split.scan_ns, 700, "decoding is part of scanning and stays there");
        assert_eq!(split.total(), 1000);
    }

    #[test]
    fn a_probe_gives_back_the_build_and_the_copies() {
        let mut document = Document::new("select 1");
        let mut stages = Spent::of(Stage::Build, 400, 0);
        stages.add(Spent::of(Stage::Materialize, 250, 0));
        stages.add(Spent::of(Stage::Strings, 50, 0));
        document.operators.push(operator("Probe", 1000, stages));
        document.operators.push(operator("Gather", 100, Spent::none()));
        document.operators.push(operator("Aggregate", 80, Spent::none()));
        document.operators.push(operator("Sort", 20, Spent::none()));
        document.timing.result_ns = 5;
        let split = document.split();
        assert_eq!(
            split,
            Split {
                build_ns: 500,
                probe_ns: 300,
                strings_ns: 50,
                materialize_ns: 255,
                aggregate_ns: 80,
                other_ns: 20,
                ..Split::default()
            }
        );
    }

    #[test]
    fn stages_that_add_up_to_more_than_the_operator_leave_it_nothing_rather_than_wrapping() {
        let mut document = Document::new("select 1");
        document.operators.push(operator("Filter", 10, Spent::of(Stage::Strings, 40, 0)));
        let split = document.split();
        assert_eq!(split.strings_ns, 40);
        assert_eq!(split.filter_ns, 0);
    }

    #[test]
    fn the_parts_are_named_in_one_order() {
        let names: Vec<&str> = Split::default().parts().iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            ["scan", "filter", "build", "probe", "aggregate", "strings", "materialize", "other"]
        );
    }
}
