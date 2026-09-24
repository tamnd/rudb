//! The search path, which is where a name written without a database or a schema is looked for and
//! where a `CREATE` of one goes.
//!
//! `SET schema`, `SET search_path` and `USE` all write it, and each entry is a schema with the
//! database it is in when that was written. An entry with no database is in the default one. The
//! text is read the way the pin reads it, so a dot splits a database from a schema, a comma splits
//! one entry from the next, and double quotes keep either as part of a name.

use rudb_common::{Error, Result};

/// One entry of the search path: the database, empty when none was written, and the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchEntry {
    /// The database, or empty for the default one.
    pub catalog: String,
    /// The schema inside it.
    pub schema: String,
}

impl SearchEntry {
    /// The entry the way `current_setting('search_path')` writes it.
    #[must_use]
    pub fn text(&self) -> String {
        if self.catalog.is_empty() {
            quoted(&self.schema)
        } else {
            format!("{}.{}", quoted(&self.catalog), quoted(&self.schema))
        }
    }
}

/// Every entry of a written search path, none for an empty one.
///
/// # Errors
///
/// For an empty part, more than two parts in one entry, or a quote that is never closed, in the
/// pin's words.
pub fn parse_list(text: &str) -> Result<Vec<SearchEntry>> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    let mut out = Vec::new();
    while at < chars.len() {
        out.push(parse_entry(&chars, &mut at)?);
    }
    Ok(out)
}

/// The one entry a `SET schema` names.
///
/// # Errors
///
/// For what [`parse_list`] refuses, and for text that holds more than one entry.
pub fn parse_one(text: &str) -> Result<SearchEntry> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    let entry = parse_entry(&chars, &mut at)?;
    if at < chars.len() {
        return Err(Error::parser(format!(
            "Failed to convert entry \"{text}\" to CatalogSearchEntry - expected a single entry"
        )));
    }
    Ok(entry)
}

/// One entry from `at`, leaving `at` past the comma that ended it.
fn parse_entry(chars: &[char], at: &mut usize) -> Result<SearchEntry> {
    let mut parts: Vec<String> = Vec::new();
    loop {
        let mut part = String::new();
        let mut last = true;
        while *at < chars.len() {
            match chars[*at] {
                '"' => {
                    *at += 1;
                    loop {
                        let Some(&c) = chars.get(*at) else {
                            return Err(Error::parser("Unterminated quote in qualified name!"));
                        };
                        *at += 1;
                        if c != '"' {
                            part.push(c);
                        } else if chars.get(*at) == Some(&'"') {
                            part.push('"');
                            *at += 1;
                        } else {
                            break;
                        }
                    }
                }
                '.' => {
                    last = false;
                    break;
                }
                ',' => break,
                c => {
                    part.push(c);
                    *at += 1;
                }
            }
        }
        if part.is_empty() {
            return Err(Error::parser("Unexpected dot - empty CatalogSearchEntry"));
        }
        parts.push(part);
        if parts.len() > 2 {
            return Err(Error::parser(
                "Too many dots - expected [schema] or [catalog.schema] for CatalogSearchEntry",
            ));
        }
        *at += 1;
        if last {
            break;
        }
    }
    let schema = parts.pop().unwrap_or_default();
    Ok(SearchEntry { catalog: parts.pop().unwrap_or_default(), schema })
}

/// A name in double quotes when it holds a dot, a comma or a quote, which are the three characters
/// the reading above would take apart, and as it is otherwise.
fn quoted(name: &str) -> String {
    if name.contains(['.', ',', '"']) {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(catalog: &str, schema: &str) -> SearchEntry {
        SearchEntry { catalog: catalog.to_string(), schema: schema.to_string() }
    }

    #[test]
    fn a_path_is_read_the_way_the_pin_reads_it() {
        assert_eq!(parse_list("s2,s1").unwrap(), [entry("", "s2"), entry("", "s1")]);
        assert_eq!(parse_list("memory.s1").unwrap(), [entry("memory", "s1")]);
        assert_eq!(parse_list("\"a.b\".\"c\"\"d\"").unwrap(), [entry("a.b", "c\"d")]);
        assert!(parse_list("").unwrap().is_empty());
        assert_eq!(
            parse_list("a..b").unwrap_err().message(),
            "Unexpected dot - empty CatalogSearchEntry"
        );
        assert!(parse_list("a.b.c").unwrap_err().message().starts_with("Too many dots"));
        assert!(parse_one("a,b").unwrap_err().message().starts_with("Failed to convert entry"));
        assert_eq!(entry("a.b", "c\"d").text(), "\"a.b\".\"c\"\"d\"");
    }
}
