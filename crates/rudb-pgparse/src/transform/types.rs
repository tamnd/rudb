//! Type names, as the text that a cast carries.
//!
//! The grammar gives the type names of SQL in their `pg_catalog` form, so `int` is
//! `pg_catalog.int4` and `timestamp with time zone` is `pg_catalog.timestamptz`. The text keeps
//! that form, and the binder resolves it the way it resolves any qualified type name.

use super::{Made, Transform, clause, not_yet};
use crate::Category;
use crate::nodes::{Node, TypeName};

impl Transform<'_> {
    /// The text of a type name: the parts with dots between them, the modifiers in parentheses and
    /// the array bounds in brackets. A part that is not a plain lower case word is quoted, and so
    /// is a name of one part that is a keyword other than an unreserved one, such as `"char"`. This
    /// is the rule of `quote_identifier` in PostgreSQL.
    pub(super) fn type_name(&self, name: &TypeName) -> Made<String> {
        if name.setof {
            return clause("SetOf");
        }
        if name.pct_type {
            return clause("PercentType");
        }
        let parts = Self::strings(&name.names)?;
        if parts.last() == Some(&"interval") && !name.typmods.is_empty() {
            return clause("IntervalFields");
        }
        let single = parts.len() == 1;
        let mut text = parts
            .iter()
            .map(|part| {
                let plain = part.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                    && part
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                let reserved = single
                    && crate::keyword(part)
                        .is_some_and(|word| word.category != Category::Unreserved);
                if plain && !reserved {
                    (*part).to_string()
                } else {
                    format!("\"{}\"", part.replace('"', "\"\""))
                }
            })
            .collect::<Vec<_>>()
            .join(".");
        if !name.typmods.is_empty() {
            let mut modifiers = Vec::with_capacity(name.typmods.len());
            for node in name.typmods.iter().flatten() {
                match node {
                    Node::A_Const(constant) => match &constant.val {
                        Some(Node::Integer(value)) => modifiers.push(value.to_string()),
                        _ => return clause("TypeModifier"),
                    },
                    node => return Err(not_yet(node)),
                }
            }
            text.push('(');
            text.push_str(&modifiers.join(","));
            text.push(')');
        }
        for node in name.arrayBounds.iter().flatten() {
            match node {
                Node::Integer(bound) if *bound < 0 => text.push_str("[]"),
                Node::Integer(bound) => text.push_str(&format!("[{bound}]")),
                node => return Err(not_yet(node)),
            }
        }
        Ok(text)
    }
}
