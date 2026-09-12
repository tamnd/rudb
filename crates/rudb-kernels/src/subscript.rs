//! Subscripting: what `x[2]` and `x[1:2]` answer.
//!
//! Two functions, `array_extract` for one index and `array_slice` for a range, over the two things
//! that can be subscripted: a string, which is indexed by character, and a list. Both were measured
//! against `v2.0.0-dev84237` case by case rather than reasoned about, because almost none of it is
//! what a reader would guess. Indices are one based. An index off the end of a list is null and an
//! index off the end of a string is the empty string. A negative index counts from the end, so
//! `x[-1]` is the last element. A range clamps rather than raising, so `[1, 2, 3][2:99]` is the last
//! two elements and `[1, 2, 3][99:99]` is empty.
//!
//! The clamping is not symmetric and that asymmetry is the whole of the rule. A begin below one
//! moves up to one, and an end past the length moves down to the length, but a negative end that is
//! still negative after counting back from the end stays where it is, which is why
//! `'abcdef'[-99:-98]` is empty rather than the first character: the end lands at -92 and every
//! range whose end is before its begin is empty.
//!
//! A step is a list only feature. `'abcdef'[1:6:2]` is a not implemented error upstream, with a
//! suggested rewrite in it, and this says the same thing in the same words. A step of zero is an
//! invalid input error, and on a string the refusal of the step comes first.
//!
//! Everything here is one value at a time and there is no vectorized loop above it. That is the
//! rule in this crate's own documentation rather than an omission: a kernel gets a loop when a
//! profile asks for one, and nothing on the ClickBench board or in the TPC queries subscripts a
//! column, so a call over a column counts itself in [`crate::fallback`] and the report says how
//! often it happened.

use rudb_common::{Error, Result, Value};

/// What upstream says about a step on a string, including the unbalanced parenthesis in the rewrite
/// it suggests, which is upstream's and not a typo here.
const STEPPED_STRING: &str = "Slice with steps has not been implemented for string types, you can \
     consider rewriting your query as follows:\n SELECT array_to_string((str_split(string, \
     '')[begin:end:step], '');";

/// `array_extract(target, index)` on one row.
pub(crate) fn extract(target: &Value, index: &Value) -> Result<Value> {
    let index = whole(index)?;
    match target {
        Value::Varchar(text) => {
            let length = text.chars().count() as i128;
            match at(length, index) {
                // An index outside a string is the empty string and not a null, which is the one
                // place the two sides of this file disagree about what a miss is.
                None => Ok(Value::Varchar(String::new())),
                Some(found) => {
                    let character = text
                        .chars()
                        .nth(found)
                        .map_or_else(String::new, |character| character.to_string());
                    Ok(Value::Varchar(character))
                }
            }
        }
        Value::List { values, .. } => match at(values.len() as i128, index) {
            None => Ok(Value::Null),
            Some(found) => Ok(values[found].clone()),
        },
        other => Err(Error::internal(format!("a subscript of a {}", other.logical_type()))),
    }
}

/// `array_slice(target, begin, end)` and `array_slice(target, begin, end, step)` on one row.
pub(crate) fn slice(
    target: &Value,
    begin: &Value,
    end: &Value,
    step: Option<&Value>,
) -> Result<Value> {
    let step = match step {
        Some(written) => Some(whole(written)?),
        None => None,
    };
    let (begin, end) = (whole(begin)?, whole(end)?);
    match target {
        Value::Varchar(text) => {
            if step.is_some() {
                return Err(Error::not_implemented(STEPPED_STRING));
            }
            let characters: Vec<char> = text.chars().collect();
            let kept: String = indices(characters.len() as i128, begin, end, None)?
                .map(|at| characters[at])
                .collect();
            Ok(Value::Varchar(kept))
        }
        Value::List { element, values } => {
            let kept = indices(values.len() as i128, begin, end, step)?
                .map(|at| values[at].clone())
                .collect();
            Ok(Value::List { element: element.clone(), values: kept })
        }
        other => Err(Error::internal(format!("a slice of a {}", other.logical_type()))),
    }
}

/// The number an index or a bound is, which the binder has already cast to a BIGINT.
fn whole(value: &Value) -> Result<i128> {
    value
        .as_i64()
        .map(i128::from)
        .ok_or_else(|| Error::internal(format!("a subscript by a {}", value.logical_type())))
}

/// One index into a value of `length` elements, zero based, or `None` for an index that misses.
///
/// A negative index counts back from the end and a zero is a miss, because the first element is at
/// one. `[1, 2, 3][0]` and `[1, 2, 3][4]` are both null upstream and `[1, 2, 3][-1]` is 3.
fn at(length: i128, index: i128) -> Option<usize> {
    let found = if index < 0 { length + index + 1 } else { index };
    if found < 1 || found > length {
        return None;
    }
    usize::try_from(found - 1).ok()
}

/// Every index a range keeps, zero based, in the order the answer holds them.
///
/// Without a step this is a run. With one it is a walk, and the walk keeps the stride the query
/// asked for rather than starting where the value happens to be legal: `[1, 2, 3, 4, 5][-99:99:2]`
/// is the odd numbered elements upstream, so a begin below one moves up by whole steps and a begin
/// past the end moves down by whole steps when the step is negative. That is also what keeps the
/// walk short when a bound is enormous, since the loop itself never visits an index it discards.
///
/// # Errors
///
/// If the step is zero, which is upstream's own sentence.
fn indices(
    length: i128,
    begin: i128,
    end: i128,
    step: Option<i128>,
) -> Result<impl Iterator<Item = usize>> {
    let from_end = |bound: i128| if bound < 0 { length + bound + 1 } else { bound };
    let (first, last) = (from_end(begin), from_end(end));
    let (start, stop, stride) = match step {
        None => (first.max(1), last.min(length), 1),
        Some(0) => return Err(Error::invalid_input("Slice step cannot be zero")),
        Some(step) if step > 0 => {
            let start = if first < 1 { first + step * up(1 - first, step) } else { first };
            (start, last.min(length), step)
        }
        Some(step) => {
            let stride = -step;
            // The end is not clamped down to the length here, and that is the measured difference
            // between `[99:1:-1]`, which is the whole list reversed, and `[99:99:-1]`, which is
            // empty: the begin walks back into the list and the end stays out past it.
            let start =
                if first > length { first - stride * up(first - length, stride) } else { first };
            (start, last.max(1), step)
        }
    };
    let mut at = start;
    Ok(std::iter::from_fn(move || {
        let done = if stride > 0 { at > stop } else { at < stop };
        if done {
            return None;
        }
        let found = usize::try_from(at - 1).ok();
        at += stride;
        found
    }))
}

/// `gap` divided by `stride`, rounded up, both being positive.
fn up(gap: i128, stride: i128) -> i128 {
    (gap + stride - 1) / stride
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;

    fn list(values: &[i64]) -> Value {
        Value::List {
            element: LogicalType::BigInt,
            values: values.iter().copied().map(Value::BigInt).collect(),
        }
    }

    fn numbers(value: &Value) -> Vec<i64> {
        match value {
            Value::List { values, .. } => {
                values.iter().map(|held| held.as_i64().unwrap()).collect()
            }
            other => panic!("not a list: {other}"),
        }
    }

    fn text(value: &Value) -> String {
        match value {
            Value::Varchar(held) => held.clone(),
            other => panic!("not a string: {other}"),
        }
    }

    /// Every answer here was read off `v2.0.0-dev84237`.
    #[test]
    fn an_index_counts_from_one_and_a_negative_one_counts_from_the_end() {
        let held = list(&[1, 2, 3]);
        let index = |at: i64| extract(&held, &Value::BigInt(at)).expect("extracts");
        assert_eq!(index(2), Value::BigInt(2));
        assert_eq!(index(-1), Value::BigInt(3));
        assert_eq!(index(-3), Value::BigInt(1));
        // Off either end of a list is null, and off either end of a string is the empty string.
        assert_eq!(index(0), Value::Null);
        assert_eq!(index(4), Value::Null);
        assert_eq!(index(-4), Value::Null);
        let word = Value::Varchar("abcdef".to_owned());
        let letter = |at: i64| text(&extract(&word, &Value::BigInt(at)).expect("extracts"));
        assert_eq!(letter(2), "b");
        assert_eq!(letter(-1), "f");
        assert_eq!(letter(0), "");
        assert_eq!(letter(9), "");
    }

    /// Characters and not bytes, which is the same rule `length` follows and not the one `strlen`
    /// does.
    #[test]
    fn a_string_is_indexed_by_character() {
        let word = Value::Varchar("héllo".to_owned());
        assert_eq!(text(&extract(&word, &Value::BigInt(2)).expect("extracts")), "é");
        let sliced = slice(&word, &Value::BigInt(2), &Value::BigInt(3), None).expect("slices");
        assert_eq!(text(&sliced), "él");
    }

    /// Every answer here was read off `v2.0.0-dev84237`.
    #[test]
    fn a_range_clamps_at_the_begin_and_at_the_end_but_not_the_same_way() {
        let held = list(&[1, 2, 3, 4, 5]);
        let range = |from: i64, to: i64| {
            numbers(&slice(&held, &Value::BigInt(from), &Value::BigInt(to), None).expect("slices"))
        };
        assert_eq!(range(2, 4), vec![2, 3, 4]);
        assert_eq!(range(-3, -1), vec![3, 4, 5]);
        assert_eq!(range(0, 2), vec![1, 2]);
        assert_eq!(range(2, 99), vec![2, 3, 4, 5]);
        assert_eq!(range(-99, 99), vec![1, 2, 3, 4, 5]);
        // The three empty ones, and the last is the asymmetry: an end that is still negative after
        // counting back from the end stays negative and every range like that is empty.
        assert!(range(4, 2).is_empty());
        assert!(range(99, 99).is_empty());
        assert!(range(-99, -98).is_empty());
        // The bounds the transformer writes for a range that left one out, which have to answer the
        // whole of the value.
        assert_eq!(range(1, -1), vec![1, 2, 3, 4, 5]);
    }

    /// Every answer here was read off `v2.0.0-dev84237`.
    #[test]
    fn a_step_walks_the_range_and_a_negative_one_walks_it_backwards() {
        let held = list(&[1, 2, 3, 4, 5]);
        let walk = |from: i64, to: i64, by: i64| {
            let step = Value::BigInt(by);
            let sliced = slice(&held, &Value::BigInt(from), &Value::BigInt(to), Some(&step));
            numbers(&sliced.expect("slices"))
        };
        assert_eq!(walk(1, 5, 2), vec![1, 3, 5]);
        assert_eq!(walk(2, 5, 2), vec![2, 4]);
        assert_eq!(walk(5, 1, -1), vec![5, 4, 3, 2, 1]);
        assert_eq!(walk(5, 1, -2), vec![5, 3, 1]);
        assert_eq!(walk(-1, -3, -1), vec![5, 4, 3]);
        // A begin outside the value walks back into it by whole steps, which is why the first of
        // these keeps the odd elements rather than the even ones.
        assert_eq!(walk(-99, 99, 2), vec![1, 3, 5]);
        assert_eq!(walk(99, 1, -1), vec![5, 4, 3, 2, 1]);
        assert_eq!(walk(3, -99, -1), vec![3, 2, 1]);
        // And the empty ones, of which the second is the end staying out past the value.
        assert!(walk(2, 4, -1).is_empty());
        assert!(walk(99, 99, -1).is_empty());
        assert!(walk(5, 2, 2).is_empty());
    }

    #[test]
    fn a_step_of_zero_and_a_step_on_a_string_are_both_refused() {
        let zero = Value::BigInt(0);
        let held = list(&[1, 2, 3]);
        let error = slice(&held, &Value::BigInt(1), &Value::BigInt(3), Some(&zero))
            .expect_err("a step of zero is refused");
        assert_eq!(error.message(), "Slice step cannot be zero");
        // On a string the step is refused before its value is looked at, so the zero above says
        // something different here.
        let word = Value::Varchar("abcdef".to_owned());
        let error = slice(&word, &Value::BigInt(1), &Value::BigInt(3), Some(&zero))
            .expect_err("a step on a string is refused");
        assert!(
            error.message().starts_with("Slice with steps has not been implemented"),
            "{error}"
        );
    }
}
