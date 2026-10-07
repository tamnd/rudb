//! The storage options that PostgreSQL accepts in `CREATE TABLE ... WITH (...)`.
//!
//! The options tell the PostgreSQL heap how to fill its pages and when autovacuum runs. This
//! engine has neither, so a PostgreSQL session checks each option the way `reloptions.c` does and
//! then drops it. The check is the part a client sees: a dump or a tool such as `pgbench -i` writes
//! these options, and a wrong name or value must give the error that PostgreSQL gives.
//!
//! The names, the kinds and the bounds were read from PostgreSQL 19 through psql.

use rudb_common::{Error, Result, SqlState};

/// The kind of value an option takes, with its bounds.
#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Int(i32, i32),
    Real(f64, f64),
    /// `vacuum_index_cleanup`, which is `auto` or a boolean.
    IndexCleanup,
}

/// One option: the name, the kind, and whether a `toast.` option of the name exists too.
struct Known {
    name: &'static str,
    kind: Kind,
    toast: bool,
}

const fn option(name: &'static str, kind: Kind, toast: bool) -> Known {
    Known { name, kind, toast }
}

const MAX: i32 = i32::MAX;

/// The table options in the order of `reloptions.c`.
const OPTIONS: &[Known] = &[
    option("autovacuum_enabled", Kind::Bool, true),
    option("user_catalog_table", Kind::Bool, false),
    option("vacuum_truncate", Kind::Bool, true),
    option("vacuum_index_cleanup", Kind::IndexCleanup, true),
    option("fillfactor", Kind::Int(10, 100), false),
    option("autovacuum_vacuum_threshold", Kind::Int(0, MAX), true),
    option("autovacuum_vacuum_max_threshold", Kind::Int(-1, MAX), true),
    option("autovacuum_vacuum_insert_threshold", Kind::Int(-1, MAX), true),
    option("autovacuum_analyze_threshold", Kind::Int(0, MAX), false),
    option("autovacuum_vacuum_cost_limit", Kind::Int(1, 10000), true),
    option("autovacuum_freeze_min_age", Kind::Int(0, 1_000_000_000), true),
    option("autovacuum_multixact_freeze_min_age", Kind::Int(0, 1_000_000_000), true),
    option("autovacuum_freeze_max_age", Kind::Int(100_000, 2_000_000_000), true),
    option("autovacuum_multixact_freeze_max_age", Kind::Int(10_000, 2_000_000_000), true),
    option("autovacuum_freeze_table_age", Kind::Int(0, 2_000_000_000), true),
    option("autovacuum_multixact_freeze_table_age", Kind::Int(0, 2_000_000_000), true),
    option("log_autovacuum_min_duration", Kind::Int(-1, MAX), true),
    option("log_autoanalyze_min_duration", Kind::Int(-1, MAX), false),
    option("toast_tuple_target", Kind::Int(128, 8160), false),
    option("parallel_workers", Kind::Int(0, 1024), false),
    option("autovacuum_parallel_workers", Kind::Int(-1, 1024), false),
    option("autovacuum_vacuum_cost_delay", Kind::Real(0.0, 100.0), true),
    option("autovacuum_vacuum_scale_factor", Kind::Real(0.0, 100.0), true),
    option("autovacuum_vacuum_insert_scale_factor", Kind::Real(0.0, 100.0), true),
    option("autovacuum_analyze_scale_factor", Kind::Real(0.0, 100.0), false),
    option("vacuum_max_eager_freeze_failure_rate", Kind::Real(0.0, 1.0), true),
];

/// One option as it was written: the namespace, the name, and the value as text.
///
/// An option with no value has the value `true`, as in PostgreSQL.
pub(crate) struct Written {
    pub(crate) namespace: Option<String>,
    pub(crate) name: String,
    pub(crate) value: Option<String>,
}

/// Check the options of a table and give the error of PostgreSQL for the first bad one.
///
/// PostgreSQL first refuses an unknown namespace, then reads `oids`, then checks the options of
/// the heap in the order they were written, and then the options of the TOAST table.
pub(crate) fn check_table(options: &[Written]) -> Result<()> {
    for written in options {
        if let Some(namespace) = &written.namespace
            && namespace != "toast"
        {
            return Err(invalid(format!("unrecognized parameter namespace \"{namespace}\"")));
        }
    }
    for written in options {
        if written.namespace.is_none() && written.name == "oids" {
            let value = written.value.as_deref().unwrap_or("true");
            if parse_bool(value) != Some(false) {
                return Err(Error::not_implemented("tables declared WITH OIDS are not supported")
                    .state(SqlState::FEATURE_NOT_SUPPORTED));
            }
        }
    }
    check_set(options.iter().filter(|w| w.namespace.is_none() && w.name != "oids"), false)?;
    check_set(options.iter().filter(|w| w.namespace.is_some()), true)
}

fn check_set<'w>(options: impl Iterator<Item = &'w Written>, toast: bool) -> Result<()> {
    let mut seen = Vec::new();
    for written in options {
        let Some(known) = OPTIONS.iter().find(|o| o.name == written.name && (o.toast || !toast))
        else {
            return Err(invalid(format!("unrecognized parameter \"{}\"", written.name)));
        };
        if seen.contains(&known.name) {
            return Err(invalid(format!("parameter \"{}\" specified more than once", known.name)));
        }
        seen.push(known.name);
        check_value(known, written.value.as_deref().unwrap_or("true"))?;
    }
    Ok(())
}

fn check_value(known: &Known, value: &str) -> Result<()> {
    let name = known.name;
    let bad = |kind: &str| invalid(format!("invalid value for {kind} option \"{name}\": {value}"));
    let out_of_bounds = |low: String, high: String| {
        invalid(format!("value {value} out of bounds for option \"{name}\""))
            .detail(format!("Valid values are between \"{low}\" and \"{high}\"."))
    };
    match known.kind {
        Kind::Bool => parse_bool(value).map(drop).ok_or_else(|| bad("boolean")),
        Kind::IndexCleanup => {
            if value.eq_ignore_ascii_case("auto") || parse_bool(value).is_some() {
                Ok(())
            } else {
                Err(bad("enum").detail("Valid values are \"on\", \"off\", and \"auto\"."))
            }
        }
        Kind::Int(low, high) => {
            let parsed = parse_int(value).ok_or_else(|| bad("integer"))?;
            if parsed < low || parsed > high {
                return Err(out_of_bounds(low.to_string(), high.to_string()));
            }
            Ok(())
        }
        Kind::Real(low, high) => {
            let parsed = parse_real(value).ok_or_else(|| bad("floating point"))?;
            if parsed < low || parsed > high {
                return Err(out_of_bounds(format!("{low:.6}"), format!("{high:.6}")));
            }
            Ok(())
        }
    }
}

fn invalid(message: String) -> Error {
    Error::invalid_input(message).state(SqlState::INVALID_PARAMETER_VALUE)
}

/// `parse_bool` of PostgreSQL: a prefix of `true`, `false`, `yes` or `no`, `on`, a prefix of
/// `off` of two letters or more, `1` or `0`, in any case. Spaces are not taken off.
fn parse_bool(value: &str) -> Option<bool> {
    let lower = value.to_ascii_lowercase();
    let prefix = |word: &str, least: usize| lower.len() >= least && word.starts_with(&lower);
    if prefix("true", 1) || prefix("yes", 1) || lower == "on" || lower == "1" {
        Some(true)
    } else if prefix("false", 1) || prefix("no", 1) || prefix("off", 2) || lower == "0" {
        Some(false)
    } else {
        None
    }
}

/// `parse_int` of PostgreSQL with no units: an integer as `strtol` with base 0 reads it, so a
/// `0x` prefix is hexadecimal and a leading `0` is octal, or else a number with a fraction or an
/// exponent, rounded. Spaces around the value are allowed.
fn parse_int(value: &str) -> Option<i32> {
    let text = value.trim_matches(|c: char| c.is_ascii_whitespace());
    if let Some(parsed) = parse_c_integer(text) {
        return i32::try_from(parsed).ok();
    }
    let real = parse_c_real(text)?;
    let rounded = real.round_ties_even();
    if rounded.is_nan() || rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        return None;
    }
    Some(rounded as i32)
}

/// `parse_real` of PostgreSQL with no units.
fn parse_real(value: &str) -> Option<f64> {
    let real = parse_c_real(value.trim_matches(|c: char| c.is_ascii_whitespace()))?;
    real.is_finite().then_some(real)
}

fn parse_c_integer(text: &str) -> Option<i64> {
    let (negative, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (radix, digits) =
        if let Some(hex) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
            (16, hex)
        } else if digits.len() > 1 && digits.starts_with('0') {
            (8, &digits[1..])
        } else {
            (10, digits)
        };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    // A value too large for an `i64` is too large for an `i32` too.
    let parsed = i64::from_str_radix(digits, radix).unwrap_or(i64::MAX);
    Some(if negative { -parsed } else { parsed })
}

fn parse_c_real(text: &str) -> Option<f64> {
    // `str::parse` takes `inf` and `nan` the way `strtod` does, and it refuses a bare `.` and an
    // empty text.
    text.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The namespace, the name and the value of an option, with an empty namespace for none.
    type Written3<'a> = (&'a str, &'a str, Option<&'a str>);

    fn check(options: &[Written3<'_>]) -> std::result::Result<(), String> {
        let options: Vec<Written> = options
            .iter()
            .map(|&(namespace, name, value)| Written {
                namespace: (!namespace.is_empty()).then(|| namespace.to_string()),
                name: name.to_string(),
                value: value.map(str::to_string),
            })
            .collect();
        check_table(&options).map_err(|error| {
            let detail = error.fields().and_then(|f| f.detail.clone()).unwrap_or_default();
            format!("{} {}|{detail}", error.reported_state().as_str(), error.message())
        })
    }

    #[test]
    fn the_options_of_a_table_give_the_errors_of_postgres() {
        assert_eq!(check(&[("", "fillfactor", Some("100"))]), Ok(()));
        assert_eq!(check(&[("", "fillfactor", Some("0x46"))]), Ok(()));
        assert_eq!(check(&[("", "fillfactor", Some(" 1e2 "))]), Ok(()));
        assert_eq!(check(&[("toast", "autovacuum_enabled", Some("of"))]), Ok(()));
        assert_eq!(check(&[("", "vacuum_index_cleanup", Some("AUTO"))]), Ok(()));
        assert_eq!(check(&[("", "oids", Some("false"))]), Ok(()));
        assert_eq!(check(&[("", "log_autovacuum_min_duration", Some("-1"))]), Ok(()));
        let cases: &[(&[Written3<'_>], &str)] = &[
            (&[("", "foo", Some("1"))], "22023 unrecognized parameter \"foo\"|"),
            (&[("foo", "a", None)], "22023 unrecognized parameter namespace \"foo\"|"),
            (&[("toast", "fillfactor", Some("5"))], "22023 unrecognized parameter \"fillfactor\"|"),
            (
                &[("", "fillfactor", Some("5"))],
                "22023 value 5 out of bounds for option \"fillfactor\"|Valid values are between \"10\" and \"100\".",
            ),
            (
                &[("", "fillfactor", Some("1.5"))],
                "22023 value 1.5 out of bounds for option \"fillfactor\"|Valid values are between \"10\" and \"100\".",
            ),
            (
                &[("", "fillfactor", None)],
                "22023 invalid value for integer option \"fillfactor\": true|",
            ),
            (
                &[("", "fillfactor", Some("99999999999"))],
                "22023 invalid value for integer option \"fillfactor\": 99999999999|",
            ),
            (
                &[("", "autovacuum_enabled", Some(" on"))],
                "22023 invalid value for boolean option \"autovacuum_enabled\":  on|",
            ),
            (
                &[("", "autovacuum_vacuum_cost_delay", Some("-0.5"))],
                "22023 value -0.5 out of bounds for option \"autovacuum_vacuum_cost_delay\"|Valid values are between \"0.000000\" and \"100.000000\".",
            ),
            (
                &[("", "autovacuum_vacuum_cost_delay", Some("1ms"))],
                "22023 invalid value for floating point option \"autovacuum_vacuum_cost_delay\": 1ms|",
            ),
            (
                &[("", "vacuum_index_cleanup", Some("x"))],
                "22023 invalid value for enum option \"vacuum_index_cleanup\": x|Valid values are \"on\", \"off\", and \"auto\".",
            ),
            (
                &[("", "fillfactor", Some("50")), ("", "fillfactor", Some("60"))],
                "22023 parameter \"fillfactor\" specified more than once|",
            ),
            (
                &[("", "foo", Some("1")), ("", "fillfactor", Some("5"))],
                "22023 unrecognized parameter \"foo\"|",
            ),
            (&[("", "oids", None)], "0A000 tables declared WITH OIDS are not supported|"),
        ];
        for (options, expected) in cases {
            assert_eq!(check(options).unwrap_err(), *expected);
        }
    }
}
