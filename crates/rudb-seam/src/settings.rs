//! The three ways of reaching a seam, which are one mechanism wearing three hats.

use std::collections::BTreeMap;

use rudb_common::{Error, Result};

use crate::policy::{Policy, PolicyMode};
use crate::seam::SeamId;

/// The prefix a seam key carries in the SQL settings surface.
///
/// `SET seam.hash.table = 'unchained'`. The prefix is there because several seam names are bare
/// words that a future DuckDB setting could reasonably want, and a compatibility surface that
/// collides with its own extension points is a compatibility surface that gets changed later.
pub const SEAM_PREFIX: &str = "seam.";

/// What the session has been told about seam selection.
///
/// Three surfaces reach this and they agree because they are the same code. A process flag on the
/// command line, a `SET` in a session, and a per query hint all end up in
/// [`Settings::set`]. A researcher who learns one of them has learned all three.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    mode: PolicyMode,
    pins: BTreeMap<SeamId, String>,
}

impl Settings {
    /// Nothing pinned, the default mode, which is what a fresh session gets.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every seam at its reference implementation, which is what the oracle runs under.
    #[must_use]
    pub fn reference() -> Self {
        Self { mode: PolicyMode::Reference, pins: BTreeMap::new() }
    }

    /// Apply one setting, spelled the way a user spells it.
    ///
    /// The key is a seam name with or without the [`SEAM_PREFIX`], and the value is the name of a
    /// registered implementation. The key `policy` is the seam that chooses at every other seam,
    /// so `seam.policy = 'reference'` is how the whole engine is put into reference mode, and it
    /// goes through this one function like everything else.
    ///
    /// The value `default` clears a pin rather than pinning something called `default`, which is
    /// the only special case and it exists so that a session can undo itself.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Catalog`](rudb_common::ErrorCode::Catalog) when nothing is called that, with
    /// the list of seams in the message, because a mistyped seam name is the commonest way to get
    /// a run that measured the wrong thing.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let name = key.strip_prefix(SEAM_PREFIX).unwrap_or(key);
        let Some(seam) = SeamId::from_name(name) else {
            return Err(Error::catalog(format!(
                "no seam called {name}, see rudb_strategies() for the list"
            )));
        };

        if seam == SeamId::Policy {
            let Some(mode) = PolicyMode::from_name(value) else {
                return Err(Error::invalid_input(format!(
                    "policy is reference, default or adaptive-bandit, not {value}"
                )));
            };
            self.mode = mode;
            return Ok(());
        }

        if value == "default" {
            self.pins.remove(&seam);
        } else {
            self.pins.insert(seam, value.to_string());
        }
        Ok(())
    }

    /// What a setting currently reads back as.
    ///
    /// An unpinned seam reads back as `default` rather than as nothing, because that is what
    /// setting it to `default` would leave it as and a settings surface that does not round trip
    /// is a settings surface somebody reports as a bug.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        let name = key.strip_prefix(SEAM_PREFIX).unwrap_or(key);
        let seam = SeamId::from_name(name)?;
        if seam == SeamId::Policy {
            return Some(self.mode.to_string());
        }
        Some(self.pins.get(&seam).cloned().unwrap_or_else(|| "default".to_string()))
    }

    /// The session mode.
    #[must_use]
    pub fn mode(&self) -> PolicyMode {
        self.mode
    }

    /// Set the session mode directly, for a caller that has one in hand already.
    pub fn set_mode(&mut self, mode: PolicyMode) {
        self.mode = mode;
    }

    /// Pin one seam to one implementation by name.
    pub fn pin(&mut self, seam: SeamId, name: impl Into<String>) {
        self.pins.insert(seam, name.into());
    }

    /// Remove a pin, leaving the seam on its default rule.
    pub fn unpin(&mut self, seam: SeamId) {
        self.pins.remove(&seam);
    }

    /// What this seam is pinned to, if anything.
    #[must_use]
    pub fn pinned(&self, seam: SeamId) -> Option<&str> {
        self.pins.get(&seam).map(String::as_str)
    }

    /// Every pin, in seam order, which is what a metrics document records.
    pub fn pins(&self) -> impl Iterator<Item = (SeamId, &str)> {
        self.pins.iter().map(|(seam, name)| (*seam, name.as_str()))
    }

    /// The policy for one seam, which is the mode and the pins resolved against each other.
    ///
    /// Reference mode beats a pin. Everything else loses to one.
    #[must_use]
    pub fn policy_for(&self, seam: SeamId) -> Policy<'_> {
        if self.mode == PolicyMode::Reference {
            return Policy::Reference;
        }
        if let Some(name) = self.pinned(seam) {
            return Policy::Pinned(name);
        }
        match self.mode {
            PolicyMode::Reference => Policy::Reference,
            PolicyMode::Default => Policy::Default,
            PolicyMode::Adaptive => Policy::Adaptive,
        }
    }
}
