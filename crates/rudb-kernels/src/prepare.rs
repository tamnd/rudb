//! The half of a scalar call that depends on the query rather than on the chunk.
//!
//! A kernel is handed a vector and a function name and works out everything else from scratch. Most
//! of what it works out is the same on every chunk, because it comes from a literal the user wrote:
//! the pattern of a `LIKE`, the pattern and the options of a regular expression, the part a
//! `date_part` reads. A pipeline over `hits` runs a hundred thousand chunks, so anything decided
//! per chunk is decided a hundred thousand times for one query, and compiling a regular expression
//! is not a cheap thing to do a hundred thousand times.
//!
//! A [`Recipe`] is that work done once. The caller that builds a pipeline knows which arguments are
//! literals, because it has the plan in front of it, so it hands them here and gets back a call it
//! can run per chunk with the deciding already finished.
//!
//! # Why this is not a cache
//!
//! A cache keyed on the pattern would also compile once, and it would cost a hash and a lock on
//! every chunk to find out that nothing changed, and it would be wrong the first time somebody runs
//! two pipelines that use two patterns on two threads with a cache of one entry. The information is
//! already sitting in the plan. Reading it there is both cheaper and simpler than rediscovering it.
//!
//! # Adding one
//!
//! The `Hoisted` enum is the list of what has been lifted so far, and it is deliberately short. A
//! function earns a variant when the work it repeats per chunk is worth more than the branch that
//! asks whether it was hoisted, which in practice means the function compiles something. The rule a
//! new variant has to keep is that hoisting changes nothing a query can see: a literal that does not
//! compile stays unhoisted rather than failing here, so the error still comes out of the chunk that
//! reaches it and reads exactly as it did before.

use rudb_common::Value;

use crate::regexp;
use crate::scalar;

/// A scalar call with whatever does not change from chunk to chunk already worked out.
///
/// Build one with [`Recipe::new`] when the pipeline is built, then run it per chunk with
/// [`call_prepared`](crate::scalar::call_prepared).
#[derive(Debug)]
pub struct Recipe {
    /// The resolved function name, so a caller carries one thing rather than two.
    name: String,
    hoisted: Hoisted,
}

/// What a recipe managed to lift out of the per chunk path.
#[derive(Debug)]
pub(crate) enum Hoisted {
    /// Nothing, either because this function has no prepare step or because the argument that would
    /// drive it is not a literal. The kernel does what it always did, which for the functions below
    /// means deciding per chunk and for everything else means there was never anything to decide.
    Nothing,
    /// A compiled `LIKE` pattern, already folded to lower case where the spelling folds case.
    Like(scalar::Like),
    /// A compiled regular expression, with the replacement taken apart and the options read.
    ///
    /// Boxed because it is several times the size of the other variants and one function node in
    /// four hundred is a regular expression.
    Regexp(Box<regexp::Call>),
}

impl Recipe {
    /// What this call can work out from the arguments that are literals.
    ///
    /// `literals` holds one entry per argument, which is the value where the argument is a literal
    /// and `None` where it is anything else. An argument that is a literal in the plan arrives as a
    /// constant vector holding that value on every chunk, so what is read here is what the kernel
    /// would have read per chunk.
    #[must_use]
    pub fn new(name: &str, literals: &[Option<Value>]) -> Self {
        let hoisted = scalar::hoist(name, literals).unwrap_or(Hoisted::Nothing);
        Self { name: name.to_owned(), hoisted }
    }

    /// A call with nothing hoisted, for a caller with no plan to read literals out of.
    #[must_use]
    pub fn plain(name: &str) -> Self {
        Self { name: name.to_owned(), hoisted: Hoisted::Nothing }
    }

    /// The function this calls.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether anything was lifted out of the per chunk path.
    ///
    /// The answer a query gives is the same either way, which is the whole point, so this is what a
    /// test has to look at to say the lifting happened at all.
    #[must_use]
    pub fn hoists(&self) -> bool {
        !matches!(self.hoisted, Hoisted::Nothing)
    }

    /// What was lifted, for the kernels that look.
    pub(crate) fn hoisted(&self) -> &Hoisted {
        &self.hoisted
    }
}

impl Hoisted {
    /// The compiled `LIKE`, or `None` when this call has none and the kernel should compile its own.
    pub(crate) fn like(&self) -> Option<&scalar::Like> {
        match self {
            Self::Like(like) => Some(like),
            _ => None,
        }
    }

    /// The compiled regular expression, or `None` for the same reason.
    pub(crate) fn regexp(&self) -> Option<&regexp::Call> {
        match self {
            Self::Regexp(call) => Some(call),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use super::Recipe;

    fn text(spelling: &str) -> Option<Value> {
        Some(Value::Varchar(spelling.to_owned()))
    }

    #[test]
    fn a_like_against_a_literal_pattern_is_compiled_here() {
        let recipe = Recipe::new("~~", &[None, text("%google%")]);
        assert!(recipe.hoisted().like().is_some());
        assert_eq!(recipe.name(), "~~");
    }

    #[test]
    fn a_pattern_that_is_not_a_literal_is_left_to_the_chunk() {
        // Legal SQL and vanishingly rare, and the point is that it still runs. The per chunk path
        // reads the pattern off the vector, and where the vector is not constant either it falls
        // all the way through to the row at a time loop and counts itself there.
        assert!(Recipe::new("~~", &[None, None]).hoisted().like().is_none());
    }

    #[test]
    fn a_regular_expression_against_a_literal_pattern_is_compiled_here() {
        let recipe = Recipe::new("regexp_matches", &[None, text("^a.*z$")]);
        assert!(recipe.hoisted().regexp().is_some());
    }

    #[test]
    fn a_pattern_that_does_not_compile_is_left_to_the_chunk() {
        // The one rule a prepare step has to keep. Compiling early must not move an error earlier,
        // because a query that raises while a pipeline is being built raises before the rows it
        // would have raised on, and in the case of a pattern under a `CASE` arm it raises on rows
        // that were never going to reach it.
        let recipe = Recipe::new("regexp_matches", &[None, text("a(")]);
        assert!(recipe.hoisted().regexp().is_none());
    }

    #[test]
    fn a_function_with_nothing_to_lift_lifts_nothing() {
        let recipe = Recipe::new("upper", &[None]);
        assert!(recipe.hoisted().like().is_none());
        assert!(recipe.hoisted().regexp().is_none());
    }

    #[test]
    fn a_plain_recipe_is_the_name_and_no_more() {
        let recipe = Recipe::plain("~~");
        assert_eq!(recipe.name(), "~~");
        assert!(recipe.hoisted().like().is_none());
    }
}
