//! One registry per seam, built at process start and immutable afterwards.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use rudb_common::{Error, Result};

use crate::context::Context;
use crate::policy::{ChoiceReason, Policy};
use crate::seam::SeamId;
use crate::strategy::{Determinism, Provenance, Strategy};

/// Every implementation of one seam.
///
/// Registration is explicit and central, one `register.rs` per crate listing everything, rather
/// than by inventory or by a linker trick. The reason is that the list is a public artifact:
/// `rudb_strategies()` returns it, `EXPLAIN` names from it and the sweep enumerates it, and a
/// mechanism that discovers implementations magically produces a different list on a different
/// platform. Somebody adding an implementation adds one line to that file, and the fact that they
/// had to is the feature.
#[derive(Debug)]
pub struct Registry<T: ?Sized + Strategy> {
    seam: SeamId,
    entries: Vec<Box<T>>,
    reference: usize,
    default: usize,
}

impl<T: ?Sized + Strategy> Registry<T> {
    /// Start building the registry for one seam.
    #[must_use]
    pub fn builder(seam: SeamId) -> RegistryBuilder<T> {
        RegistryBuilder { seam, entries: Vec::new(), reference: None, default: None }
    }

    /// Which seam this is.
    #[must_use]
    pub fn seam(&self) -> SeamId {
        self.seam
    }

    /// How many implementations are registered, which is never zero.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Always false. Present because clippy asks for it next to [`Registry::len`] and because a
    /// caller writing a generic report should not have to know that a registry with no reference
    /// cannot be built.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every implementation, in registration order, which is the order the sweep runs them in.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(Box::as_ref)
    }

    /// The obviously correct one. Never deleted, never the fast one.
    #[must_use]
    pub fn reference(&self) -> &T {
        self.entries[self.reference].as_ref()
    }

    /// The one the default rule picks when it is applicable.
    #[must_use]
    pub fn default(&self) -> &T {
        self.entries[self.default].as_ref()
    }

    /// The implementation with this name, or `None` if nothing is called that.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&T> {
        self.iter().find(|entry| entry.name() == name)
    }

    /// Which implementation runs, and why.
    ///
    /// The why is half the value. A number that came from a pin somebody left in their shell and a
    /// number that came from the rule everybody gets are different numbers, and telling them apart
    /// after the fact is impossible unless the choice recorded its own reason at the time.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Catalog`](rudb_common::ErrorCode::Catalog) when a pin names something that is
    /// not registered, and [`ErrorCode::InvalidInput`](rudb_common::ErrorCode::InvalidInput) when
    /// it names something that is registered and cannot handle this situation. Both are errors
    /// rather than a quiet fall back, because a run that did not do what the setting asked for is
    /// a run whose number says something other than what it means.
    pub fn choose(&self, context: &Context<'_>) -> Result<Choice<'_, T>> {
        if context.seam() != self.seam {
            return Err(Error::internal(format!(
                "context for {} used to choose at {}",
                context.seam(),
                self.seam
            )));
        }

        match context.settings().policy_for(self.seam) {
            Policy::Reference => {
                Ok(Choice { strategy: self.reference(), reason: ChoiceReason::Reference })
            }
            Policy::Pinned(name) => {
                let Some(strategy) = self.by_name(name) else {
                    return Err(Error::catalog(format!(
                        "{} has no implementation called {name}, it has {}",
                        self.seam,
                        self.names().join(", ")
                    )));
                };
                if !strategy.applicable(context) {
                    return Err(Error::invalid_input(format!(
                        "{} is pinned to {name} and {name} cannot run here",
                        self.seam
                    )));
                }
                Ok(Choice { strategy, reason: ChoiceReason::Pinned })
            }
            Policy::Default => {
                let fallback = self.default();
                if fallback.applicable(context) {
                    return Ok(Choice { strategy: fallback, reason: ChoiceReason::Default });
                }
                match self.iter().find(|entry| entry.applicable(context)) {
                    Some(strategy) => Ok(Choice { strategy, reason: ChoiceReason::Fallback }),
                    None => Err(Error::internal(format!(
                        "nothing registered at {} can run here, including the reference",
                        self.seam
                    ))),
                }
            }
            Policy::Adaptive => Err(Error::not_implemented(
                "the adaptive policy is F10, set seam.policy to default or reference".to_string(),
            )),
        }
    }

    /// Every registered name, for an error message or a sweep argument.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.iter().map(Strategy::name).collect()
    }

    /// One row per implementation, which is what `rudb_strategies()` prints.
    #[must_use]
    pub fn rows(&self) -> Vec<StrategyRow> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StrategyRow {
                seam: self.seam,
                name: entry.name(),
                describe: entry.describe(),
                provenance: entry.provenance(),
                determinism: entry.deterministic(),
                is_reference: index == self.reference,
                is_default: index == self.default,
            })
            .collect()
    }
}

/// Builds a [`Registry`], checking the things a registry cannot be built without.
#[derive(Debug)]
pub struct RegistryBuilder<T: ?Sized + Strategy> {
    seam: SeamId,
    entries: Vec<Box<T>>,
    reference: Option<usize>,
    default: Option<&'static str>,
}

impl<T: ?Sized + Strategy> RegistryBuilder<T> {
    /// Register the reference implementation. Exactly one seam entry is this.
    #[must_use]
    pub fn reference(mut self, entry: Box<T>) -> Self {
        self.reference = Some(self.entries.len());
        self.entries.push(entry);
        self
    }

    /// Register an alternative. Named this rather than `add` because a builder method called
    /// `add` reads as arithmetic to clippy and to about half of the people who see it.
    #[must_use]
    pub fn alternative(mut self, entry: Box<T>) -> Self {
        self.entries.push(entry);
        self
    }

    /// Name the one the default rule starts from. Without this the reference is the default, which
    /// is the honest state for a seam whose alternatives have not been measured yet.
    #[must_use]
    pub fn default_is(mut self, name: &'static str) -> Self {
        self.default = Some(name);
        self
    }

    /// Finish.
    ///
    /// # Panics
    ///
    /// When no reference was registered, when two implementations share a name, or when the named
    /// default is not registered. All three are mistakes in a `register.rs` rather than anything a
    /// user can cause, they are found the first time the process starts, and a panic here is a
    /// clearer report than an engine that runs with a seam that cannot be selected at.
    #[must_use]
    pub fn build(self) -> Registry<T> {
        let seam = self.seam;
        let reference = self
            .reference
            .unwrap_or_else(|| panic!("seam {seam} was registered without a reference"));

        let mut seen: Vec<&'static str> = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            assert!(
                !seen.contains(&entry.name()),
                "seam {seam} has two entries called {}",
                entry.name()
            );
            seen.push(entry.name());
        }

        let default = match self.default {
            None => reference,
            Some(name) => {
                self.entries.iter().position(|entry| entry.name() == name).unwrap_or_else(|| {
                    panic!("seam {seam} defaults to {name}, which is not registered")
                })
            }
        };

        Registry { seam, entries: self.entries, reference, default }
    }
}

/// The implementation that runs and the reason it is the one.
#[derive(Debug)]
pub struct Choice<'a, T: ?Sized + Strategy> {
    strategy: &'a T,
    reason: ChoiceReason,
}

impl<'a, T: ?Sized + Strategy> Choice<'a, T> {
    /// The implementation.
    #[must_use]
    pub fn strategy(&self) -> &'a T {
        self.strategy
    }

    /// Why it is the one.
    #[must_use]
    pub fn reason(&self) -> ChoiceReason {
        self.reason
    }
}

impl<T: ?Sized + Strategy> Deref for Choice<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.strategy
    }
}

impl<T: ?Sized + Strategy> fmt::Display for Choice<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.strategy.name(), self.reason)
    }
}

/// One line of `rudb_strategies()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrategyRow {
    /// The seam it is an implementation of.
    pub seam: SeamId,
    /// Its stable name.
    pub name: &'static str,
    /// What it does, in one line.
    pub describe: &'static str,
    /// Where it came from.
    pub provenance: Provenance,
    /// Whether it gives the same answer twice.
    pub determinism: Determinism,
    /// Whether it is the reference.
    pub is_reference: bool,
    /// Whether the default rule starts from it.
    pub is_default: bool,
}

/// A registry with its element type erased, so that one list can hold all of them.
///
/// `rudb_strategies()` needs every seam in one table and the seams do not share an element type, so
/// something has to forget it. This is that something, and it deliberately exposes only the parts
/// that are the same for every seam.
pub trait RegistryView: Send + Sync + fmt::Debug {
    /// Which seam.
    fn seam(&self) -> SeamId;

    /// One row per registered implementation.
    fn rows(&self) -> Vec<StrategyRow>;
}

impl<T: ?Sized + Strategy> RegistryView for Registry<T> {
    fn seam(&self) -> SeamId {
        Registry::seam(self)
    }

    fn rows(&self) -> Vec<StrategyRow> {
        Registry::rows(self)
    }
}

/// Every registry in the process, which is what the settings surface and the table function read.
///
/// Most seams are not in here yet. That is the honest state of a project at F0 and
/// [`Registries::unregistered`] is how it gets printed rather than hidden.
#[derive(Debug, Clone, Default)]
pub struct Registries {
    views: BTreeMap<SeamId, Arc<dyn RegistryView>>,
}

impl Registries {
    /// Nothing registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one seam's registry.
    ///
    /// # Panics
    ///
    /// When a seam is registered twice, which is a mistake in a `register.rs`.
    pub fn add(&mut self, view: Arc<dyn RegistryView>) {
        let seam = view.seam();
        assert!(self.views.insert(seam, view).is_none(), "seam {seam} was registered twice");
    }

    /// Every row of every registry, in seam order.
    #[must_use]
    pub fn rows(&self) -> Vec<StrategyRow> {
        self.views.values().flat_map(|view| view.rows()).collect()
    }

    /// The seams that have no registry yet, in the order the design lists them.
    ///
    /// Each of these has a milestone that owes it, which is [`SeamId::milestone`], and printing the
    /// two together is how a reader finds out what is planned as against what is built.
    #[must_use]
    pub fn unregistered(&self) -> Vec<SeamId> {
        SeamId::ALL.iter().copied().filter(|seam| !self.views.contains_key(seam)).collect()
    }

    /// Whether this seam has a registry.
    #[must_use]
    pub fn has(&self, seam: SeamId) -> bool {
        self.views.contains_key(&seam)
    }
}
