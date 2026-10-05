//! A macro made with `CREATE MACRO` or `CREATE FUNCTION`, which is a body of text with names in it
//! that a call puts its arguments in place of.

use rudb_parse::quoted;

use crate::QualifiedName;

/// One parameter of a macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// Its name, which the body uses and a named argument gives.
    pub name: String,
    /// The type it was written with, which picks between overloads, or `None` for any type.
    pub ty: Option<String>,
    /// The text of its default, which a call that leaves it out gets, or `None` if it has none.
    pub default: Option<String>,
}

/// One way of calling a macro, which is its own parameters and its own body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overload {
    /// The parameters, the ones with a default after the ones without.
    pub parameters: Vec<Parameter>,
    /// The body as the pin prints it back, which is also the text a call expands.
    pub body: String,
    /// Whether the body has an aggregate in it, which makes a query that calls it a grouping one.
    pub aggregating: bool,
}

impl Overload {
    /// The line the pin prints for it among the candidates when a call fits none of them, such as
    /// `m(a, b := 10)` or `m(x INTEGER)`.
    #[must_use]
    pub fn signature(&self, name: &str) -> String {
        let parameters: Vec<String> = self
            .parameters
            .iter()
            .map(|parameter| {
                let mut text = parameter.name.clone();
                if let Some(ty) = &parameter.ty {
                    text += &format!(" {ty}");
                }
                if let Some(default) = &parameter.default {
                    text += &format!(" := {default}");
                }
                text
            })
            .collect();
        format!("{name}({})", parameters.join(", "))
    }
}

/// A macro, which lives in a schema the way a table does but among the functions, so that a table
/// and a macro can have one name. A scalar macro and a table macro are two entries as well, and
/// one name can be both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Macro {
    /// Its full name.
    pub name: QualifiedName,
    /// Whether it is a table macro, called in a `FROM`, rather than a scalar one.
    pub table: bool,
    /// The ways it can be called, in the order they were written.
    pub overloads: Vec<Overload>,
    /// The number the catalog tables join on, given when it is made.
    pub oid: i64,
}

impl Macro {
    /// The word the pin's messages use for it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        kind(self.table)
    }

    /// The statement that would make it again under its bare name, which is what a database file
    /// keeps of it.
    #[must_use]
    pub fn sql(&self) -> String {
        let overloads: Vec<String> = self
            .overloads
            .iter()
            .map(|overload| {
                let parameters: Vec<String> = overload
                    .parameters
                    .iter()
                    .map(|parameter| {
                        let mut text = quoted(&parameter.name);
                        if let Some(ty) = &parameter.ty {
                            text += &format!(" {ty}");
                        }
                        if let Some(default) = &parameter.default {
                            text += &format!(" := {default}");
                        }
                        text
                    })
                    .collect();
                let table = if self.table { "TABLE " } else { "" };
                format!("({}) AS {table}{}", parameters.join(", "), overload.body)
            })
            .collect();
        format!("CREATE MACRO {}{}", quoted(&self.name.table), overloads.join(", "))
    }
}

/// The word the pin's messages use for a scalar macro or a table one.
#[must_use]
pub fn kind(table: bool) -> &'static str {
    if table { "Table Macro Function" } else { "Macro Function" }
}
