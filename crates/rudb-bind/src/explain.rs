//! The options of a PostgreSQL `EXPLAIN`, read the way `ParseExplainOptionList` reads them.
//!
//! PostgreSQL binds the query before it reads the options, so an error in the query is reported
//! before an error in the options, and the caller keeps that order. The messages, the SQLSTATEs and
//! the positions are the ones of `explain_state.c` and of `defGetBoolean` and `defGetString` in
//! `define.c`.

use rudb_common::{Error, Result, SqlState};
use rudb_parse::Ast;
use rudb_parse::ast::{OptionArg, Slice, UtilityOption};
use rudb_plan::explain::{Format, Options, Serialize};

/// The options written in `options`, with `analyze` set first when the statement was written in
/// the DuckDB grammar as `EXPLAIN ANALYZE`.
pub(crate) fn options(ast: &Ast, options: Slice, analyze: bool) -> Result<Options> {
    let mut read = Options { analyze, ..Options::default() };
    let mut timing = None;
    let mut buffers = None;
    let mut summary = None;
    for option in &ast.utility_options[options.range()] {
        let name = ast.string(option.name);
        match name {
            "analyze" => read.analyze = boolean(ast, option)?,
            "verbose" => read.verbose = boolean(ast, option)?,
            "costs" => read.costs = boolean(ast, option)?,
            "buffers" => buffers = Some(boolean(ast, option)?),
            "wal" => read.wal = boolean(ast, option)?,
            "settings" => read.settings = boolean(ast, option)?,
            "generic_plan" => read.generic = boolean(ast, option)?,
            "timing" => timing = Some(boolean(ast, option)?),
            "summary" => summary = Some(boolean(ast, option)?),
            "memory" => read.memory = boolean(ast, option)?,
            "serialize" => {
                read.serialize = match option.arg {
                    // `SERIALIZE` with no value is taken as `text`.
                    OptionArg::None => Serialize::Text,
                    _ => match string(ast, option)?.as_str() {
                        "off" | "none" => Serialize::None,
                        "text" => Serialize::Text,
                        "binary" => Serialize::Binary,
                        value => return Err(unrecognized_value(option, name, value)),
                    },
                };
            }
            "format" => {
                read.format = match string(ast, option)?.as_str() {
                    "text" => Format::Text,
                    "xml" => Format::Xml,
                    "json" => Format::Json,
                    "yaml" => Format::Yaml,
                    value => return Err(unrecognized_value(option, name, value)),
                };
            }
            "io" => read.io = boolean(ast, option)?,
            _ => {
                return Err(Error::binder(format!("unrecognized EXPLAIN option \"{name}\""))
                    .state(SqlState::SYNTAX_ERROR)
                    .with_span(option.span));
            }
        }
    }
    if read.wal && !read.analyze {
        return Err(requires_analyze("WAL"));
    }
    read.timing = timing.unwrap_or(read.analyze);
    read.buffers = buffers.unwrap_or(read.analyze);
    if read.timing && !read.analyze {
        return Err(requires_analyze("TIMING"));
    }
    if read.io && !read.analyze {
        return Err(requires_analyze("IO"));
    }
    if read.serialize != Serialize::None && !read.analyze {
        return Err(requires_analyze("SERIALIZE"));
    }
    if read.generic && read.analyze {
        return Err(Error::binder(
            "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together",
        )
        .state(SqlState::INVALID_PARAMETER_VALUE)
        .unplaced());
    }
    read.summary = summary.unwrap_or(read.analyze);
    Ok(read)
}

/// Whether the options ask for `GENERIC_PLAN`, which `transformExplainStmt` reads before the query
/// is bound, so that the query binds with its parameters as placeholders. The last time the option
/// is written wins, and a value that is not a Boolean is reported later with the other options.
pub(crate) fn generic(ast: &Ast, options: Slice) -> bool {
    ast.utility_options[options.range()]
        .iter()
        .rev()
        .find(|option| ast.string(option.name) == "generic_plan")
        .is_some_and(|option| boolean(ast, option).unwrap_or(false))
}

/// The value of an option that takes a Boolean, as `defGetBoolean` reads it: no value is true, and
/// otherwise `0`, `1`, `true`, `false`, `on` or `off` in any case.
pub(crate) fn boolean(ast: &Ast, option: &UtilityOption) -> Result<bool> {
    let value = match option.arg {
        OptionArg::None => return Ok(true),
        OptionArg::Integer(0) => Some(false),
        OptionArg::Integer(1) => Some(true),
        OptionArg::Integer(_) => None,
        OptionArg::Word(text) | OptionArg::Number(text) => {
            let text = ast.string(text);
            if text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("on") {
                Some(true)
            } else if text.eq_ignore_ascii_case("false") || text.eq_ignore_ascii_case("off") {
                Some(false)
            } else {
                None
            }
        }
    };
    value.ok_or_else(|| {
        Error::binder(format!("{} requires a Boolean value", ast.string(option.name)))
            .state(SqlState::SYNTAX_ERROR)
            .unplaced()
    })
}

/// The value of an option as text, as `defGetString` gives it.
pub(crate) fn string(ast: &Ast, option: &UtilityOption) -> Result<String> {
    match option.arg {
        OptionArg::None => {
            Err(Error::binder(format!("{} requires a parameter", ast.string(option.name)))
                .state(SqlState::SYNTAX_ERROR)
                .unplaced())
        }
        OptionArg::Word(text) | OptionArg::Number(text) => Ok(ast.string(text).to_string()),
        OptionArg::Integer(value) => Ok(value.to_string()),
    }
}

fn unrecognized_value(option: &UtilityOption, name: &str, value: &str) -> Error {
    Error::binder(format!("unrecognized value for EXPLAIN option \"{name}\": \"{value}\""))
        .state(SqlState::INVALID_PARAMETER_VALUE)
        .with_span(option.span)
}

fn requires_analyze(option: &str) -> Error {
    Error::binder(format!("EXPLAIN option {option} requires ANALYZE"))
        .state(SqlState::INVALID_PARAMETER_VALUE)
        .unplaced()
}
