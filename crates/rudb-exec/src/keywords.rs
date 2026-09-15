//! `duckdb_keywords()`, every word the grammar knows and which class each one is in.
//!
//! The rows come out of `rudb_parse::KEYWORDS`, which is generated from the vendored grammar by
//! `cargo xtask gen-grammar` and checked against it in the gate. So this table is correct by
//! construction rather than by somebody having transcribed a list: the day the pin moves and upstream
//! adds a word, the generated table gains it and so does this.
//!
//! Two things about the shape are not obvious and both were measured against the pinned binary. A
//! word can be in two categories at once and six of them are, which is why 499 words produce 505
//! rows, and fifteen of the 514 entries in the generated table are in no category at all because the
//! grammar spells them directly in some rule. `rudb_functions::keyword_categories` is where both of
//! those live, since the mapping from the grammar's five rules to DuckDB's four categories is a fact
//! about the grammar rather than about this operator.
//!
//! The rows come out sorted by name, because the generated table is sorted so that a lookup is a
//! binary search. The pinned binary returns them grouped by category instead, in the order its
//! internal keyword lists happen to be in, and that is not reproduced. It is an implementation
//! detail of somebody else's parser rather than a fact about the language, the one record in the
//! corpus that reads this table is a `statement ok` that does not look at the rows, and a
//! sqllogictest record that cares about order says so.

use rudb_common::Result;
use rudb_functions::{keyword_categories, keyword_fields};
use rudb_parse::KEYWORDS;
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every keyword and its category, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn keywords(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let mut rows = Vec::with_capacity(KEYWORDS.len());
    for (word, classes) in KEYWORDS {
        for category in keyword_categories(classes) {
            rows.push(vec![text(word), text(category)]);
        }
    }
    Metadata::new("duckdb_keywords", &keyword_fields(), &rows, plan, index, columns)
}
