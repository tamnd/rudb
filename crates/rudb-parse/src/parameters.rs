//! The parameter names of the built-in functions that have them, and the order a call written with
//! `name := value` arguments puts its arguments in.
//!
//! Most of the pin's functions call their parameters `col0`, `col1` and so on, which no query can
//! name. The ones below are the rest, read out of `duckdb_functions()` on the pinned binary, plus
//! `lead` and `lag`, which are window functions and so are not in that listing. Each entry is every
//! distinct list of names the function's overloads have, types left out. A trailing parameter may
//! have a default, which is written into the call when a later parameter is named and this one is
//! not, so `lead("default" := 0, col := x)` is `lead(x, 1, 0)`.
//!
//! The names are matched without regard to case, the way the pin matches them.

/// One parameter, with the SQL of the value it takes when it is left out, if it can be.
#[derive(Debug, Clone, Copy)]
pub struct Parameter {
    /// The name a call can give the argument.
    pub name: &'static str,
    /// The value it takes when the call leaves it out, as SQL, or `None` when it must be given.
    pub default: Option<&'static str>,
}

const fn p(name: &'static str) -> Parameter {
    Parameter { name, default: None }
}

const fn d(name: &'static str, default: &'static str) -> Parameter {
    Parameter { name, default: Some(default) }
}

const SORT: &[&[Parameter]] =
    &[&[p("list")], &[p("list"), p("sort_order")], &[p("list"), p("sort_order"), p("null_order")]];
const REVERSE_SORT: &[&[Parameter]] = &[&[p("list")], &[p("list"), p("null_order")]];
const QUANTILE: &[&[Parameter]] = &[&[p("x"), p("quantile")], &[p("x")]];
const CONCAT: &[&[Parameter]] = &[&[p("input")], &[p("input"), p("separator")]];
const ROUND: &[&[Parameter]] = &[&[p("x"), p("precision")]];
const PART: &[&[Parameter]] = &[&[p("part_list"), p("ts")]];
const PARSE: &[&[Parameter]] = &[&[p("text"), p("format")]];
const TRANSFORM: &[&[Parameter]] = &[&[p("json"), p("structure")]];
const JSON_PATH: &[&[Parameter]] = &[&[p("json"), p("path")]];
const SEQUENCE: &[&[Parameter]] = &[&[p("sequence_name")]];
const EXTRACT: &[&[Parameter]] = &[&[p("list"), p("index")]];
const MATCH: &[&[Parameter]] =
    &[&[p("string"), p("regex")], &[p("string"), p("regex"), p("options")]];
const NEIGHBOUR: &[&[Parameter]] = &[&[p("col"), d("offset", "1"), d("default", "NULL")]];

const REGEXP_EXTRACT_ALL: &[&[Parameter]] = &[
    &[p("string"), p("regex")],
    &[p("string"), p("regex"), p("group")],
    &[p("string"), p("regex"), p("group"), p("options")],
    &[p("string"), p("regex"), p("name_list")],
    &[p("string"), p("regex"), p("name_list"), p("options")],
];

const SERIALIZE: &[Parameter] = &[
    p("sql"),
    d("skip_null", "false"),
    d("skip_empty", "false"),
    d("skip_default", "false"),
    d("format", "false"),
];

const PARAMETERS: &[(&str, &[&[Parameter]])] = &[
    ("approx_quantile", &[&[p("x"), p("quantile")]]),
    (
        "array_extract",
        &[&[p("array"), p("index")], &[p("struct"), p("key")], &[p("tuple"), p("index")]],
    ),
    ("array_grade_up", SORT),
    ("array_reverse_sort", REVERSE_SORT),
    ("array_sort", SORT),
    ("bitstring_agg", &[&[p("arg")], &[p("arg"), p("min"), p("max")]]),
    ("current_schemas", &[&[p("include_implicit")]]),
    ("current_setting", &[&[p("setting_name")]]),
    ("currval", SEQUENCE),
    ("date_part", PART),
    ("datepart", PART),
    (
        "decimal_division",
        &[&[p("numerator"), p("denominator")], &[p("numerator"), p("denominator"), p("scale")]],
    ),
    ("duckdb_format_sql", &[&[p("sql")], &[p("sql"), p("config")]]),
    ("from_json", TRANSFORM),
    ("getvariable", &[&[p("variable_name")]]),
    ("grade_up", SORT),
    ("group_concat", CONCAT),
    ("icu_sort_key", &[&[p("str"), p("collator")]]),
    ("json_array_length", JSON_PATH),
    ("json_keys", JSON_PATH),
    (
        "json_serialize_plan",
        &[&[
            p("sql"),
            d("skip_null", "false"),
            d("skip_empty", "false"),
            d("skip_default", "false"),
            d("format", "false"),
            d("optimize", "false"),
        ]],
    ),
    ("json_serialize_sql", &[SERIALIZE]),
    ("json_transform", TRANSFORM),
    ("lag", NEIGHBOUR),
    ("lead", NEIGHBOUR),
    ("list_element", EXTRACT),
    ("list_extract", EXTRACT),
    ("list_grade_up", SORT),
    ("list_reverse_sort", REVERSE_SORT),
    ("list_sort", SORT),
    ("listagg", CONCAT),
    ("median", &[&[p("x")]]),
    ("nextval", SEQUENCE),
    ("parse_duckdb_log_message", &[&[p("type"), p("message")]]),
    ("quantile", QUANTILE),
    ("quantile_cont", &[&[p("x"), p("quantile")]]),
    ("quantile_disc", QUANTILE),
    (
        "regexp_extract",
        &[
            &[p("string"), p("regex")],
            &[p("string"), p("regex"), p("group")],
            &[p("string"), p("regex"), p("options")],
            &[p("string"), p("regex"), p("group"), p("options")],
            &[p("string"), p("regex"), p("name_list")],
            &[p("string"), p("regex"), p("name_list"), p("options")],
        ],
    ),
    ("regexp_extract_all", REGEXP_EXTRACT_ALL),
    ("regexp_full_match", MATCH),
    ("regexp_matches", MATCH),
    (
        "regexp_replace",
        &[
            &[p("string"), p("regex"), p("replacement")],
            &[p("string"), p("regex"), p("replacement"), p("options")],
        ],
    ),
    ("remap_struct", &[&[p("input"), p("target_type"), p("mapping"), p("defaults")]]),
    ("reservoir_quantile", &[&[p("x"), p("quantile")], &[p("x"), p("quantile"), p("sample_size")]]),
    ("round", &[&[p("x")], &[p("x"), p("precision")]]),
    ("round_even", ROUND),
    ("roundbankers", ROUND),
    ("strftime", &[&[p("data"), p("format")], &[p("format"), p("data")]]),
    ("string_agg", CONCAT),
    ("strptime", PARSE),
    ("struct_extract", &[&[p("struct"), p("key")], &[p("tuple"), p("index")]]),
    ("struct_extract_at", &[&[p("struct"), p("index")]]),
    (
        "to_aggregate_state",
        &[
            &[p("data"), p("name"), p("signature")],
            &[p("data"), p("name"), p("signature"), p("constant_parameters")],
            &[p("data"), p("name"), p("signature"), p("constant_parameters"), p("order_by")],
        ],
    ),
    ("try_strptime", PARSE),
    ("union_extract", &[&[p("union"), p("tag")]]),
];

/// The parameter lists of a function, or none when no overload of it has names.
pub fn lists(function: &str) -> &'static [&'static [Parameter]] {
    PARAMETERS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(function))
        .map_or(&[], |(_, lists)| *lists)
}

/// What fills one place in a call once its named arguments are in their places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The argument at this index, counting the implicit places first, then the positional
    /// arguments, then the named ones in the order they were written.
    Written(usize),
    /// A parameter that was left out, with the SQL of its default.
    Default(&'static str),
}

/// How a call's named arguments fit the function's parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arranged {
    /// One order fits, with what goes in each place.
    Slots(Vec<Slot>),
    /// No list of parameters takes these names, which the binder reports with the argument types.
    Unmatched,
    /// Two lists take them in different orders, which the pin will not choose between.
    Ambiguous,
}

/// A name given to a parameter a positional argument has already filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refilled {
    /// The index among the named arguments of the one that was given twice.
    pub named: usize,
    /// The parameter it names, as the function declares it.
    pub parameter: &'static str,
}

/// The order a call puts its arguments in, given how many were positional and the names of the
/// rest, and counting `implicit` places at the front that the call fills some other way, the way
/// `WITHIN GROUP` fills the first one.
///
/// # Errors
///
/// A name that one of the lists gives to a place a positional argument already fills.
pub fn arrange(
    function: &str,
    implicit: usize,
    positional: usize,
    names: &[&str],
) -> Result<Arranged, Refilled> {
    let given = implicit + positional;
    let mut found: Option<Vec<Slot>> = None;
    for list in lists(function) {
        let mut slots: Vec<Option<Slot>> = vec![None; list.len()];
        if given > list.len() {
            continue;
        }
        for (place, slot) in slots.iter_mut().enumerate().take(given) {
            *slot = Some(Slot::Written(place));
        }
        let mut fits = true;
        for (index, name) in names.iter().enumerate() {
            let Some(place) = list.iter().position(|held| held.name.eq_ignore_ascii_case(name))
            else {
                fits = false;
                break;
            };
            if place < given {
                return Err(Refilled { named: index, parameter: list[place].name });
            }
            slots[place] = Some(Slot::Written(given + index));
        }
        if !fits {
            continue;
        }
        // A place left empty takes its default, and one with no default rules the list out. The
        // places after the last one filled are left off rather than written with their defaults.
        let last = slots.iter().rposition(Option::is_some).map_or(0, |place| place + 1);
        let mut arranged = Vec::with_capacity(last);
        for (slot, parameter) in slots.iter().zip(list.iter()) {
            match (slot, parameter.default) {
                (Some(slot), _) => arranged.push(*slot),
                (None, Some(default)) => arranged.push(Slot::Default(default)),
                (None, None) => {
                    fits = false;
                    break;
                }
            }
        }
        arranged.truncate(last);
        if !fits {
            continue;
        }
        match &found {
            Some(before) if *before != arranged => return Ok(Arranged::Ambiguous),
            _ => found = Some(arranged),
        }
    }
    Ok(found.map_or(Arranged::Unmatched, Arranged::Slots))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(function: &str, implicit: usize, positional: usize, names: &[&str]) -> Arranged {
        arrange(function, implicit, positional, names).expect("no name refills a place")
    }

    #[test]
    fn named_arguments_go_to_the_places_their_names_have() {
        use Slot::{Default, Written};
        assert_eq!(
            slots("round", 0, 0, &["precision", "x"]),
            Arranged::Slots(vec![Written(1), Written(0)])
        );
        assert_eq!(
            slots("ROUND", 0, 1, &["PRECISION"]),
            Arranged::Slots(vec![Written(0), Written(1)])
        );
        assert_eq!(slots("round", 0, 0, &["x"]), Arranged::Slots(vec![Written(0)]));
        assert_eq!(
            slots("lead", 0, 0, &["default", "col"]),
            Arranged::Slots(vec![Written(1), Default("1"), Written(0)])
        );
        assert_eq!(
            slots("quantile_cont", 1, 0, &["quantile"]),
            Arranged::Slots(vec![Written(0), Written(1)])
        );
    }

    #[test]
    fn names_that_fit_no_list_or_two_are_left_to_the_binder() {
        assert_eq!(slots("round", 0, 0, &["y"]), Arranged::Unmatched);
        assert_eq!(slots("lower", 0, 0, &["x"]), Arranged::Unmatched);
        assert_eq!(slots("round", 0, 0, &["precision"]), Arranged::Unmatched);
        assert_eq!(slots("strftime", 0, 0, &["data", "format"]), Arranged::Ambiguous);
    }

    #[test]
    fn a_name_for_a_place_already_filled_is_refused() {
        assert_eq!(arrange("round", 0, 1, &["x"]), Err(Refilled { named: 0, parameter: "x" }));
    }
}
