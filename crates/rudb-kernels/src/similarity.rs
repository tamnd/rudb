//! The string distances and similarities: `levenshtein`, `damerau_levenshtein`, `mismatches`,
//! `jaccard`, `jaro_similarity` and `jaro_winkler_similarity`, plus the prefix and suffix tests.
//!
//! Every one of them works on bytes and not on characters, which was measured against
//! `v2.0.0-dev84237` rather than assumed: `levenshtein('héllo', 'hello')` is 2 there because the
//! `é` is two bytes and both of them have to go, and `mismatches('héllo', 'hello')` is refused
//! because six bytes are not five. The Jaro answers follow from the same reading, so
//! `jaro_similarity('héllo', 'hello')` is 0.8222222222222223 and not the 0.8666666666666667 a
//! count of characters gives.
//!
//! `damerau_levenshtein` is the unrestricted distance, where a transposed pair can be edited again
//! afterwards, so `damerau_levenshtein('ca', 'abc')` is 2 and not the 3 that the restricted
//! version, the optimal string alignment distance, gives.
//!
//! The Jaro arithmetic is written in the order the pin's library does it, because the last digit of
//! a double depends on it: `jaro_winkler_similarity('DIXON', 'DICKSONX')` is 0.8133333333333332
//! there and the obvious order of operations prints a 3 at the end instead.

use rudb_common::{Error, Result, Value};

/// `levenshtein(a, b)` and `editdist3(a, b)`, the fewest inserts, deletes and substitutions that
/// turn one string into the other.
pub(crate) fn levenshtein(a: &Value, b: &Value) -> Result<Value> {
    let (a, b) = (bytes(a)?, bytes(b)?);
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, left) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != right);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Ok(count(previous[b.len()]))
}

/// `damerau_levenshtein(a, b)`, which is [`levenshtein`] with a swap of two neighbours as one edit.
pub(crate) fn damerau_levenshtein(a: &Value, b: &Value) -> Result<Value> {
    let (a, b) = (bytes(a)?, bytes(b)?);
    let width = b.len() + 2;
    let far = a.len() + b.len();
    let mut table = vec![0; (a.len() + 2) * width];
    table[0] = far;
    for i in 0..=a.len() {
        table[(i + 1) * width] = far;
        table[(i + 1) * width + 1] = i;
    }
    for j in 0..=b.len() {
        table[j + 1] = far;
        table[width + j + 1] = j;
    }
    // The last row each byte of `a` was seen on, which is where a transposition would start.
    let mut last_row = [0; 256];
    for i in 1..=a.len() {
        let mut last_column = 0;
        for j in 1..=b.len() {
            let (k, l) = (last_row[usize::from(b[j - 1])], last_column);
            let cost = if a[i - 1] == b[j - 1] {
                last_column = j;
                0
            } else {
                1
            };
            let best = (table[i * width + j] + cost)
                .min(table[(i + 1) * width + j] + 1)
                .min(table[i * width + j + 1] + 1)
                .min(table[k * width + l] + (i - k - 1) + 1 + (j - l - 1));
            table[(i + 1) * width + j + 1] = best;
        }
        last_row[usize::from(a[i - 1])] = i;
    }
    Ok(count(table[(a.len() + 1) * width + b.len() + 1]))
}

/// `mismatches(a, b)` and `hamming(a, b)`, the positions two strings of one length differ at.
pub(crate) fn mismatches(a: &Value, b: &Value) -> Result<Value> {
    let (a, b) = (bytes(a)?, bytes(b)?);
    if a.len() != b.len() {
        return Err(Error::invalid_input("Mismatch Function: Strings must be of equal length!"));
    }
    if a.is_empty() {
        return Err(Error::invalid_input("Mismatch Function: Strings must be of length > 0!"));
    }
    Ok(count(a.iter().zip(b).filter(|(left, right)| left != right).count()))
}

/// `jaccard(a, b)`, the bytes the two strings share over the bytes either of them has, each byte
/// counted once however often it appears.
pub(crate) fn jaccard(a: &Value, b: &Value) -> Result<Value> {
    let (a, b) = (bytes(a)?, bytes(b)?);
    if a.is_empty() || b.is_empty() {
        return Err(Error::invalid_input("Jaccard Function: An argument too short!"));
    }
    let (left, right) = (present(a), present(b));
    let shared: u32 = left.iter().zip(&right).map(|(x, y)| (x & y).count_ones()).sum();
    let either: u32 = left.iter().zip(&right).map(|(x, y)| (x | y).count_ones()).sum();
    Ok(Value::Double(f64::from(shared) / f64::from(either)))
}

/// `jaro_similarity(a, b)` and `jaro_similarity(a, b, cutoff)`. An answer under the cutoff is 0.
pub(crate) fn jaro_similarity(a: &Value, b: &Value, cutoff: Option<&Value>) -> Result<Value> {
    let similarity = jaro(bytes(a)?, bytes(b)?);
    Ok(Value::Double(kept(similarity, cutoff)?))
}

/// `jaro_winkler_similarity`, which is [`jaro_similarity`] raised by the prefix the two strings
/// share, up to four bytes of it, when the Jaro similarity is over 0.7 to start with.
pub(crate) fn jaro_winkler_similarity(
    a: &Value,
    b: &Value,
    cutoff: Option<&Value>,
) -> Result<Value> {
    let (a, b) = (bytes(a)?, bytes(b)?);
    let mut similarity = jaro(a, b);
    if similarity > 0.7 {
        let prefix = a.iter().zip(b).take(4).take_while(|(left, right)| left == right).count();
        similarity += prefix as f64 * 0.1 * (1.0 - similarity);
    }
    Ok(Value::Double(kept(similarity, cutoff)?))
}

/// `starts_with`, `prefix` and `^@` when `suffix` is false, and `ends_with` and `suffix` when it is
/// true.
pub(crate) fn affix(text: &Value, affix: &Value, suffix: bool) -> Result<Value> {
    let (text, affix) = (bytes(text)?, bytes(affix)?);
    Ok(Value::Boolean(if suffix { text.ends_with(affix) } else { text.starts_with(affix) }))
}

/// The Jaro similarity of two byte strings, which is zero when either of them is empty.
fn jaro(a: &[u8], b: &[u8]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // Two bytes match when they are equal and no further apart than this.
    let reach = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut taken = vec![false; b.len()];
    let mut matched = Vec::new();
    for (i, byte) in a.iter().enumerate() {
        let window = i.saturating_sub(reach)..(i + reach + 1).min(b.len());
        if let Some(j) = window.into_iter().find(|&j| !taken[j] && b[j] == *byte) {
            taken[j] = true;
            matched.push(*byte);
        }
    }
    if matched.is_empty() {
        return 0.0;
    }
    let order = b.iter().zip(&taken).filter(|(_, taken)| **taken).map(|(byte, _)| byte);
    let crossed = matched.iter().zip(order).filter(|(left, right)| left != right).count() / 2;
    let common = matched.len() as f64;
    let similarity =
        common / a.len() as f64 + common / b.len() as f64 + (common - crossed as f64) / common;
    similarity / 3.0
}

/// The similarity, or zero when a cutoff was given and the similarity is under it.
fn kept(similarity: f64, cutoff: Option<&Value>) -> Result<f64> {
    let Some(cutoff) = cutoff else {
        return Ok(similarity);
    };
    match cutoff {
        Value::Double(cutoff) => Ok(if similarity < *cutoff { 0.0 } else { similarity }),
        other => Err(Error::internal(format!("a similarity cutoff of {}", other.logical_type()))),
    }
}

/// The bytes that appear in a string, as a set of 256 bits.
fn present(text: &[u8]) -> [u64; 4] {
    let mut set = [0; 4];
    for byte in text {
        set[usize::from(byte / 64)] |= 1 << (byte % 64);
    }
    set
}

/// The bytes of an argument, which the binder has already cast to a VARCHAR.
fn bytes(value: &Value) -> Result<&[u8]> {
    match value {
        Value::Varchar(text) => Ok(text.as_bytes()),
        other => Err(Error::internal(format!("a string distance over a {}", other.logical_type()))),
    }
}

/// A count as the BIGINT the distances return.
fn count(count: usize) -> Value {
    Value::BigInt(i64::try_from(count).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::Varchar(value.to_string())
    }

    fn whole(value: Result<Value>) -> i64 {
        value.unwrap().as_i64().unwrap()
    }

    fn double(value: Result<Value>) -> f64 {
        match value.unwrap() {
            Value::Double(held) => held,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_distances_are_the_pins() {
        let plain =
            [("kitten", "sitting", 3), ("hallo", "hoi", 4), ("", "hi", 2), ("héllo", "hello", 2)];
        for (a, b, expected) in plain {
            assert_eq!(whole(levenshtein(&text(a), &text(b))), expected, "{a} {b}");
        }
        let swapped = [("ca", "abc", 2), ("abcdef", "badcfe", 3), ("a cat", "an abct", 3)];
        for (a, b, expected) in swapped {
            assert_eq!(whole(damerau_levenshtein(&text(a), &text(b))), expected, "{a} {b}");
        }
    }

    #[test]
    fn the_similarities_are_the_pins_to_the_last_digit() {
        let jaros = [
            ("CRATE", "TRACE", 0.7333333333333334),
            ("DIXON", "DICKSONX", 0.7666666666666666),
            ("MARTHA", "MARHTA", 0.9444444444444445),
            ("héllo", "hello", 0.8222222222222223),
            ("abc", "cba", 0.5555555555555555),
            ("a", "ab", 0.8333333333333334),
            ("ab", "ba", 0.0),
            ("abcd", "dcba", 0.5),
            ("", "", 0.0),
        ];
        for (a, b, expected) in jaros {
            assert_eq!(double(jaro_similarity(&text(a), &text(b), None)), expected, "{a} {b}");
        }
        let winklers = [
            ("DIXON", "DICKSONX", 0.8133333333333332),
            ("MARTHA", "MARHTA", 0.9611111111111111),
            ("héllo", "hello", 0.8400000000000001),
            ("abcdefgh", "abcdefgx", 0.95),
        ];
        for (a, b, expected) in winklers {
            let answer = double(jaro_winkler_similarity(&text(a), &text(b), None));
            assert_eq!(answer, expected, "{a} {b}");
        }
        let (a, b) = (text("abcdefgh"), text("abcdefgx"));
        assert_eq!(double(jaro_winkler_similarity(&a, &b, Some(&Value::Double(0.95)))), 0.95);
        assert_eq!(double(jaro_winkler_similarity(&a, &b, Some(&Value::Double(0.99)))), 0.0);
        assert_eq!(double(jaccard(&text("héllo"), &text("hello"))), 0.5);
        assert_eq!(double(jaccard(&text("aab"), &text("ab"))), 1.0);
    }

    #[test]
    fn a_string_the_function_cannot_measure_is_refused_in_the_pins_words() {
        let said = mismatches(&text("hoi"), &text("hallo")).unwrap_err().to_string();
        assert!(said.contains("Mismatch Function: Strings must be of equal length!"), "{said}");
        let said = mismatches(&text(""), &text("")).unwrap_err().to_string();
        assert!(said.contains("Mismatch Function: Strings must be of length > 0!"), "{said}");
        let said = jaccard(&text("hello"), &text("")).unwrap_err().to_string();
        assert!(said.contains("Jaccard Function: An argument too short!"), "{said}");
    }
}
