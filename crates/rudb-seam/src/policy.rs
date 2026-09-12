//! Which registered implementation runs.

use std::fmt;

/// The answer to "given a seam and a context, which implementation".
///
/// This is derived per seam by [`Settings::policy_for`](crate::Settings::policy_for) rather than
/// set directly, because the session holds one mode and a set of pins and the per seam answer
/// falls out of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Policy<'a> {
    /// Always the reference implementation.
    ///
    /// This is what the differential oracle runs under and it is the one thing that overrides a
    /// pin, because an oracle that a stray session setting can redirect is not an oracle.
    Reference,
    /// Always this one, by name.
    Pinned(&'a str),
    /// The hand written rule for the seam, which is what ships.
    Default,
    /// A contextual bandit over the applicable set.
    ///
    /// F10 fills this in. Until then asking for it is an error rather than a quiet fall back to
    /// [`Policy::Default`], because a run that silently did not do what the setting asked for is a
    /// run whose number means something other than what it says.
    Adaptive,
}

/// What the session is set to, before per seam pins are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PolicyMode {
    /// Everything runs its reference implementation.
    Reference,
    /// Every seam runs its own default rule. This is what ships.
    #[default]
    Default,
    /// F10.
    Adaptive,
}

impl PolicyMode {
    /// The name this mode is spelled with in a setting, a flag and a hint.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            PolicyMode::Reference => "reference",
            PolicyMode::Default => "default",
            PolicyMode::Adaptive => "adaptive-bandit",
        }
    }

    /// The mode with this name, or `None` if nothing is called that.
    #[must_use]
    pub fn from_name(name: &str) -> Option<PolicyMode> {
        match name {
            "reference" => Some(PolicyMode::Reference),
            "default" => Some(PolicyMode::Default),
            "adaptive-bandit" | "adaptive" => Some(PolicyMode::Adaptive),
            _ => None,
        }
    }
}

impl fmt::Display for PolicyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why the implementation that ran is the one that ran.
///
/// `EXPLAIN` prints this beside the name. A number is worth a great deal less when nobody can tell
/// whether it came from the rule everybody gets or from a pin somebody left in their shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChoiceReason {
    /// The policy asked for the reference.
    Reference,
    /// Somebody named it.
    Pinned,
    /// The seam's default rule picked it.
    Default,
    /// The default was not applicable here, so the first one that was got it.
    ///
    /// Worth its own variant rather than being folded into [`ChoiceReason::Default`], because a
    /// seam whose default is never applicable is a seam whose default rule is wrong, and that is
    /// only visible if the fall back says so.
    Fallback,
    /// The bandit picked it. F10.
    Adaptive,
}

impl ChoiceReason {
    /// The word `EXPLAIN` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ChoiceReason::Reference => "reference",
            ChoiceReason::Pinned => "pinned",
            ChoiceReason::Default => "default",
            ChoiceReason::Fallback => "fallback",
            ChoiceReason::Adaptive => "adaptive",
        }
    }
}

impl fmt::Display for ChoiceReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
