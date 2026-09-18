//! The switches that turn one optimization off, and the two that turn a whole layer off.
//!
//! `spec/stats/09-measurement.md` section 9.2 asks for a setting per rule rather than one master
//! switch, and the reason is accounting: a layer that ships twenty rules and reports one total
//! cannot say which of them earned anything, and the twenty first gets added on the strength of a
//! number the third one produced. A rule that cannot be turned off cannot be measured, and a rule
//! that has not been measured on its own is not known to be worth its complexity.
//!
//! On top of those there are two switches that turn off everything below them, because three
//! documents ask for the same ablation and it should be one implementation of one idea rather than
//! three. [`Rule::StatsAll`] is `statistics = off` from `spec/stats/09-measurement.md` section 9.3,
//! where every consumer gets `Unknown` and every operator takes the path it takes today.
//! [`Rule::GraphSections`] is `graph_sections = off` from `spec/graph/09-measurement.md` section
//! 9.2, where every query takes the hash join, the nested loop and the ordinary scan. Both runs must
//! produce identical answers, and that comparison runs on every commit rather than at a milestone.
//!
//! The two masters do not start in the same place. `statistics` starts on, because a better estimate
//! of a number the planner already needed is not a new behaviour and nobody should have to ask for
//! it. `graph_sections` starts off, which is what tamnd/rudb#760 asks for, because a stored section
//! and a new operator are a new behaviour, and a new behaviour earns its default by measuring better
//! rather than by being written.
//!
//! # Why these are not in `Settings::NAMES`
//!
//! The same reason the seam settings are not, which `crates/rudb/src/settings.rs` states: a name in
//! that list is a name `duckdb_settings()` prints, and none of these is a setting the binary we
//! claim compatibility with has ever heard of. They go through the same `SET` path anyway, because
//! a second door into the settings is a second place for a scope rule to be wrong.
//!
//! # Three spellings, one rule
//!
//! The value is a boolean, and `on` and `off` are accepted beside `true` and `false` because that
//! is how the specification documents write these switches. In SQL the value is quoted, so the
//! statement the ablation runs is `SET statistics = 'off'`, since a bare word on the right of a
//! `SET` is a column reference and the binder says so.
//!
//! `stats.presize` is the name to write in a script and the name this module canonicalizes to.
//! `stats_presize` is the one that fits through `SET` without quoting, because the statement takes
//! an identifier and DuckDB's grammar has no dot in one. `statistics` and `graph_sections` are the
//! spellings the specification documents use for the two masters, and they are here because a
//! person who has read the specification should be able to type what it says.

use crate::{Error, Result};

/// One switch.
///
/// Every statistics variant is on by default, so a fresh database behaves as it did before any of
/// this existed and an ablation is something a run asks for rather than something it inherits.
/// [`Rule::GraphSections`] is the exception and starts off, because it is a stored structure and a
/// new path through the executor rather than a better answer to a question already being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// Every statistics consumer. Off means every question answers `Stat::Unknown`.
    StatsAll,
    /// Presizing a hash aggregate from the distinct count of its grouping key.
    Presize,
    /// Choosing direct addressing over hashing when the key range is small and dense.
    DirectAddressing,
    /// Dropping the validity handling in a kernel whose input has an exact zero null count.
    ValidityFree,
    /// Ordering the conjuncts of a filter by selectivity over evaluation cost.
    FilterOrder,
    /// Seeding a top-n threshold from a certified quantile instead of from infinity.
    TopNSeed,
    /// Summing a decimal column in `i64` when its exact bounds fit, rather than in `i128`.
    NarrowArithmetic,
    /// Removing a join whose relationship is verified and whose columns nothing above it reads.
    JoinElimination,
    /// Reserving memory for an operator from what its input statistics say it will need.
    MemoryReservation,
    /// Every stored graph section. Off means the sections are not read and no plan uses one.
    GraphSections,
}

impl Rule {
    /// Every rule, in the order a report lists them.
    pub const ALL: [Self; 10] = [
        Self::StatsAll,
        Self::Presize,
        Self::DirectAddressing,
        Self::ValidityFree,
        Self::FilterOrder,
        Self::TopNSeed,
        Self::NarrowArithmetic,
        Self::JoinElimination,
        Self::MemoryReservation,
        Self::GraphSections,
    ];

    /// The canonical name, which is what a setting reads back as.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::StatsAll => "stats.all",
            Self::Presize => "stats.presize",
            Self::DirectAddressing => "stats.direct_addressing",
            Self::ValidityFree => "stats.validity_free",
            Self::FilterOrder => "stats.filter_order",
            Self::TopNSeed => "stats.top_n_seed",
            Self::NarrowArithmetic => "stats.narrow_arithmetic",
            Self::JoinElimination => "stats.join_elimination",
            Self::MemoryReservation => "stats.memory_reservation",
            Self::GraphSections => "graph.sections",
        }
    }

    /// The rule that turns this one off from above, if there is one.
    ///
    /// A per rule switch is not enough on its own: the ablation of section 9.3 is one statement and
    /// it has to reach every consumer, including the ones added after it was written.
    #[must_use]
    pub const fn master(self) -> Option<Self> {
        match self {
            Self::StatsAll | Self::GraphSections => None,
            _ => Some(Self::StatsAll),
        }
    }

    /// Whether a fresh database has this rule on.
    ///
    /// Everything does except [`Rule::GraphSections`], and the reason is in the module docs.
    #[must_use]
    pub const fn starts_on(self) -> bool {
        !matches!(self, Self::GraphSections)
    }

    /// The rule a settings key names, in any of its spellings.
    #[must_use]
    pub fn from_name(key: &str) -> Option<Self> {
        let name = canonical(key);
        Self::ALL.into_iter().find(|rule| rule.name() == name)
    }
}

/// Whether a settings name is a rule rather than one of the settings DuckDB has.
///
/// True for a misspelled rule as well as a correct one, so that `SET stats.presise = false` gets
/// the error naming the rules rather than the one naming the DuckDB settings. A mistyped switch is
/// the commonest way to get a run that measured the wrong thing, so the message has to say which
/// list to look in.
#[must_use]
pub fn looks_like_rule(key: &str) -> bool {
    let name = canonical(key);
    name.starts_with("stats.") || name.starts_with("graph.") || Rule::from_name(&name).is_some()
}

/// Every rule name, for the sentence that says what the list is.
#[must_use]
pub fn rule_names() -> String {
    Rule::ALL.map(Rule::name).join(", ")
}

/// The canonical spelling of a key, which is the dotted lowercase one.
///
/// The first underscore of an undotted name becomes the dot, so `stats_top_n_seed` and
/// `stats.top_n_seed` are one name and the underscores inside a rule's own name survive. The two
/// specification spellings are handled here because neither of them is derivable.
fn canonical(key: &str) -> String {
    let lower = key.to_ascii_lowercase();
    match lower.as_str() {
        "statistics" => return Rule::StatsAll.name().to_string(),
        "graph_sections" => return Rule::GraphSections.name().to_string(),
        _ => {}
    }
    if lower.contains('.') {
        return lower;
    }
    match lower.split_once('_') {
        Some((head, rest)) => format!("{head}.{rest}"),
        None => lower,
    }
}

/// Which switches are on, as the statements have left them.
///
/// A bitset rather than a map, because there are ten of them, because a session copies this once
/// per statement, and because the set is fixed at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules(u16);

impl Default for Rules {
    fn default() -> Self {
        Self::new()
    }
}

impl Rules {
    /// What a fresh database has, which is every statistics rule on and the graph sections off.
    ///
    /// The statistics rules are on because their whole point is that they change no answer, so a
    /// database that had to be told to use them would be a database where nobody used them. The
    /// graph sections are off because they are a stored structure that nothing writes yet and a new
    /// path through the executor when they arrive, and a new path is worth having on by default only
    /// once the measurement says it is better. G3 in tamnd/rudb#763 is where that is decided.
    #[must_use]
    pub const fn new() -> Self {
        let mut bits = 0;
        let mut index = 0;
        while index < Rule::ALL.len() {
            let rule = Rule::ALL[index];
            if rule.starts_on() {
                bits |= bit(rule);
            }
            index += 1;
        }
        Self(bits)
    }

    /// Whether this rule may fire, which is its own switch and its master's.
    #[must_use]
    pub fn enabled(self, rule: Rule) -> bool {
        match rule.master() {
            Some(master) if !self.is_set(master) => false,
            _ => self.is_set(rule),
        }
    }

    /// Whether this rule's own switch is on, ignoring its master.
    ///
    /// What a setting reads back as, because a settings surface that does not round trip is a
    /// settings surface somebody reports as a bug. [`Rules::enabled`] is what is in effect.
    #[must_use]
    pub fn is_set(self, rule: Rule) -> bool {
        self.0 & bit(rule) != 0
    }

    /// Turns one rule on or off.
    pub fn set(&mut self, rule: Rule, enabled: bool) {
        if enabled {
            self.0 |= bit(rule);
        } else {
            self.0 &= !bit(rule);
        }
    }

    /// Turns one rule on or off by name.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Catalog`](crate::ErrorCode::Catalog) when nothing is called that, with the list
    /// of rules in the message.
    pub fn set_named(&mut self, key: &str, enabled: bool) -> Result<()> {
        let Some(rule) = Rule::from_name(key) else { return Err(no_such_rule(key)) };
        self.set(rule, enabled);
        Ok(())
    }

    /// Puts one rule back where a fresh database has it, which is what `RESET` means.
    ///
    /// Not the same as setting it on, because [`Rule::GraphSections`] starts off and a reset that
    /// turned it on would be a reset that left the database somewhere it has never been.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Catalog`](crate::ErrorCode::Catalog) when nothing is called that, with the list
    /// of rules in the message.
    pub fn reset_named(&mut self, key: &str) -> Result<()> {
        let Some(rule) = Rule::from_name(key) else { return Err(no_such_rule(key)) };
        self.set(rule, rule.starts_on());
        Ok(())
    }

    /// What one rule's setting reads back as, or `None` when nothing is called that.
    #[must_use]
    pub fn named(self, key: &str) -> Option<bool> {
        Rule::from_name(key).map(|rule| self.is_set(rule))
    }

    /// Every rule and its own switch, in report order.
    ///
    /// `spec/stats/09-measurement.md` section 9.7 requires a statistics report to carry the settings
    /// state for every rule, which is this.
    pub fn states(self) -> impl Iterator<Item = (&'static str, bool)> {
        Rule::ALL.into_iter().map(move |rule| (rule.name(), self.is_set(rule)))
    }

    /// The rules that are not where a fresh database left them, which is what a run records when it
    /// says what it measured.
    pub fn changed(self) -> impl Iterator<Item = (&'static str, bool)> {
        let fresh = Self::new();
        Rule::ALL
            .into_iter()
            .filter(move |&rule| self.is_set(rule) != fresh.is_set(rule))
            .map(move |rule| (rule.name(), self.is_set(rule)))
    }
}

const fn bit(rule: Rule) -> u16 {
    1 << (rule as u16)
}

fn no_such_rule(key: &str) -> Error {
    Error::catalog(format!("no rule called {key}, the rules are {}", rule_names()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statistics_rule_starts_on_and_the_graph_sections_start_off() {
        let rules = Rules::new();
        for rule in Rule::ALL {
            if rule == Rule::GraphSections {
                assert!(!rules.enabled(rule), "the graph sections should start off");
            } else {
                assert!(rules.enabled(rule), "{} should start on", rule.name());
            }
        }
        // A fresh database has nothing to report, off switch included.
        assert_eq!(rules.changed().count(), 0);
    }

    #[test]
    fn turning_the_graph_sections_on_is_a_change_worth_reporting() {
        let mut rules = Rules::new();
        rules.set(Rule::GraphSections, true);
        assert!(rules.enabled(Rule::GraphSections));
        assert_eq!(rules.changed().collect::<Vec<_>>(), vec![("graph.sections", true)]);
    }

    #[test]
    fn the_master_turns_off_the_rules_under_it() {
        let mut rules = Rules::new();
        rules.set(Rule::StatsAll, false);
        assert!(!rules.enabled(Rule::Presize));
        assert!(!rules.enabled(Rule::NarrowArithmetic));
        // The graph sections are their own layer and their own ablation, so the statistics master
        // does not reach them either way.
        rules.set(Rule::GraphSections, true);
        assert!(rules.enabled(Rule::GraphSections));
        // The switch underneath is still where the session left it, which is what it reads back as.
        assert!(rules.is_set(Rule::Presize));
    }

    #[test]
    fn one_rule_goes_off_without_taking_the_others_with_it() {
        let mut rules = Rules::new();
        rules.set(Rule::FilterOrder, false);
        assert!(!rules.enabled(Rule::FilterOrder));
        assert!(rules.enabled(Rule::Presize));
        assert!(rules.enabled(Rule::StatsAll));
        assert_eq!(rules.changed().collect::<Vec<_>>(), vec![("stats.filter_order", false)]);
    }

    #[test]
    fn the_spellings_all_reach_the_same_rule() {
        for spelling in ["stats.all", "stats_all", "statistics", "STATISTICS", "Stats.All"] {
            assert_eq!(Rule::from_name(spelling), Some(Rule::StatsAll), "{spelling}");
        }
        for spelling in ["graph.sections", "graph_sections", "GRAPH.SECTIONS"] {
            assert_eq!(Rule::from_name(spelling), Some(Rule::GraphSections), "{spelling}");
        }
        for spelling in ["stats.top_n_seed", "stats_top_n_seed"] {
            assert_eq!(Rule::from_name(spelling), Some(Rule::TopNSeed), "{spelling}");
        }
    }

    #[test]
    fn a_name_nobody_has_is_not_a_rule() {
        assert_eq!(Rule::from_name("memory_limit"), None);
        assert_eq!(Rule::from_name("stats.presise"), None);
        assert!(!looks_like_rule("memory_limit"));
        assert!(!looks_like_rule("threads"));
        // A misspelled rule is still a rule for the purpose of choosing the error message.
        assert!(looks_like_rule("stats.presise"));
        assert!(looks_like_rule("graph_adjacency"));
    }

    #[test]
    fn setting_by_name_says_what_the_names_are() {
        let mut rules = Rules::new();
        rules.set_named("stats_presize", false).expect("a rule by its underscore spelling");
        assert!(!rules.enabled(Rule::Presize));
        assert_eq!(rules.named("stats.presize"), Some(false));

        let refused = rules.set_named("stats.presise", false).expect_err("no such rule");
        assert!(refused.to_string().contains("stats.presize"), "{refused}");
    }

    #[test]
    fn every_rule_has_its_own_bit() {
        let mut seen = Vec::new();
        for rule in Rule::ALL {
            assert!(!seen.contains(&bit(rule)), "{} shares a bit", rule.name());
            seen.push(bit(rule));
        }
    }

    #[test]
    fn a_report_lists_every_rule() {
        let states = Rules::new().states().collect::<Vec<_>>();
        assert_eq!(states.len(), Rule::ALL.len());
        assert_eq!(states[0], ("stats.all", true));
    }
}
