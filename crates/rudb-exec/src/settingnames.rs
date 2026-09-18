//! `duckdb_settings()`, every setting `SET` will take and what each one is now.
//!
//! The one metadata table whose rows are not all decided at compile time. Which settings exist, what
//! each one is for and what type it takes are static and live in `rudb_functions::settingcatalog`.
//! What each one is now belongs to whoever ran the `SET`, so it arrives in the [`Session`] the query
//! is being built with.
//!
//! A session that has never been filled in reports null rather than a default. That is the shape a
//! caller of [`crate::build`] gets, and it is the honest answer there: this crate has no database
//! behind it, so it does not know what the memory limit is and guessing would produce a table that
//! says something false about a running system. The embedding API fills the session in, so every
//! value read through `rudb` is a real one.
//!
//! `value` and `typed_value` hold the same text, which is what the pin does on 191 of its 192 rows.
//! The pin's column is a `VARIANT` and this one is a `VARCHAR`, and `rudb_functions::settingcatalog`
//! is where that is written down.

use rudb_common::{LogicalType, Result, Session, Value};
use rudb_functions::{SETTINGS, UNSET, setting_fields};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every setting and what this session has it at, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn settingnames(
    session: &Session,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::with_capacity(SETTINGS.len());
    for entry in SETTINGS {
        // A setting that is unset reads as null and not as the empty string, and so does a setting
        // in a session nobody filled in. Three of the rows are unset on a fresh connection.
        let value = match session.get(entry.name) {
            None | Some(UNSET) => Value::Null,
            Some(held) => text(held),
        };
        rows.push(vec![
            text(entry.name),
            value.clone(),
            text(entry.description),
            text(entry.input_type),
            text(entry.scope),
            aliases(entry.aliases),
            value,
        ]);
    }
    Metadata::new("duckdb_settings", &setting_fields(), &rows, plan, index, columns)
}

/// The `VARCHAR[]` of other spellings, empty for a setting that has none.
fn aliases(names: &[&'static str]) -> Value {
    Value::List {
        element: LogicalType::Varchar,
        values: names.iter().map(|name| Value::Varchar((*name).to_string())).collect(),
    }
}
