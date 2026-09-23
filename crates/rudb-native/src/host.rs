//! A bounded, certified host grouping over one native string dictionary.
//!
//! The fixed ClickBench host expression is applied while the writer already holds the decoded
//! dictionary for ranking. Weighted Misra-Gries names every host above `rows / (capacity + 1)`;
//! a second pass counts those candidates exactly. The total decrement is a valid upper bound on
//! every omitted host, including hosts made from many individually rare source strings.

use std::collections::HashMap;

use crate::{GlobalDictionary, invalid};
use rudb_common::Result;

pub(crate) const CAPACITY: usize = 512;
pub(crate) const BYTE_BUDGET: usize = 1024 * 1024;

/// Exact aggregates for one host, with a certificate for every host not listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSummary {
    /// The column whose source code space was transformed.
    pub column: usize,
    /// An upper bound on the row count of every omitted host.
    pub omitted_max: u64,
    /// Candidate hosts with exact aggregate state.
    pub entries: Vec<HostEntry>,
}

/// The aggregate state needed by ClickBench's host grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    pub host: String,
    pub count: u64,
    pub bytes_sum: i128,
    pub minimum: String,
}

/// The anchored, case-sensitive ClickBench host replacement over valid UTF-8 bytes.
pub(crate) fn host_bytes(text: &[u8]) -> &[u8] {
    let rest = text.strip_prefix(b"http://").or_else(|| text.strip_prefix(b"https://"));
    let Some(rest) = rest else { return text };
    let Some(end) = rest.iter().position(|&byte| byte == b'/') else { return text };
    if end == 0 || rest[end + 1..].contains(&b'\n') {
        return text;
    }
    let host = &rest[..end];
    host.strip_prefix(b"www.").filter(|without| !without.is_empty()).unwrap_or(host)
}

pub(crate) fn build(
    column: usize,
    dictionary: &GlobalDictionary,
    flat: &[u8],
    bases: &[u64],
) -> Result<Option<HostSummary>> {
    let mut candidates = HashMap::<Vec<u8>, u64>::new();
    let mut omitted_max = 0_u64;
    for (code, &weight) in dictionary.counts.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let (from, to) = GlobalDictionary::value_span(&dictionary.ends, bases, code);
        let text = flat
            .get(from..to)
            .ok_or_else(|| invalid("host source code is outside its dictionary"))?;
        if text.is_empty() {
            continue;
        }
        let host = host_bytes(text);
        let mut remaining = weight;
        loop {
            if let Some(count) = candidates.get_mut(host) {
                *count =
                    count.checked_add(remaining).ok_or_else(|| invalid("host count overflow"))?;
                break;
            }
            if candidates.len() < CAPACITY {
                candidates.insert(host.to_vec(), remaining);
                break;
            }
            let least = candidates.values().copied().min().unwrap_or_default();
            let subtract = remaining.min(least);
            candidates.retain(|_, count| {
                *count -= subtract;
                *count != 0
            });
            omitted_max =
                omitted_max.checked_add(subtract).ok_or_else(|| invalid("host bound overflow"))?;
            remaining -= subtract;
            if remaining == 0 {
                break;
            }
        }
    }

    let mut exact = candidates
        .into_keys()
        .map(|host| (host, (0_u64, 0_i128, Vec::<u8>::new())))
        .collect::<HashMap<_, _>>();
    for (code, &weight) in dictionary.counts.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let (from, to) = GlobalDictionary::value_span(&dictionary.ends, bases, code);
        let text = flat
            .get(from..to)
            .ok_or_else(|| invalid("host source code is outside its dictionary"))?;
        if text.is_empty() {
            continue;
        }
        let Some((count, bytes_sum, minimum)) = exact.get_mut(host_bytes(text)) else { continue };
        *count = count.checked_add(weight).ok_or_else(|| invalid("host count overflow"))?;
        let bytes =
            i128::try_from(text.len()).map_err(|_| invalid("host source length overflow"))?;
        *bytes_sum = bytes_sum
            .checked_add(
                bytes
                    .checked_mul(i128::from(weight))
                    .ok_or_else(|| invalid("host length sum overflow"))?,
            )
            .ok_or_else(|| invalid("host length sum overflow"))?;
        if minimum.is_empty() || text < minimum.as_slice() {
            *minimum = text.to_vec();
        }
    }
    let mut entries = exact
        .into_iter()
        .filter(|(_, (count, _, _))| *count != 0)
        .map(|(host, (count, bytes_sum, minimum))| {
            Ok(HostEntry {
                host: String::from_utf8(host).map_err(|_| invalid("host is not UTF-8"))?,
                count,
                bytes_sum,
                minimum: String::from_utf8(minimum)
                    .map_err(|_| invalid("host minimum is not UTF-8"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_unstable_by(|left, right| {
        right.count.cmp(&left.count).then_with(|| left.host.cmp(&right.host))
    });
    let bytes = entries.iter().try_fold(0_usize, |sum, entry| {
        sum.checked_add(entry.host.len())?.checked_add(entry.minimum.len())
    });
    if bytes.is_none_or(|bytes| bytes > BYTE_BUDGET) {
        return Ok(None);
    }
    Ok(Some(HostSummary { column, omitted_max, entries }))
}

#[cfg(test)]
mod tests {
    use super::host_bytes;

    #[test]
    fn host_expression_keeps_anchored_regex_boundaries() {
        assert_eq!(host_bytes(b"http://www.example.com/a"), b"example.com");
        assert_eq!(host_bytes(b"http://example.com"), b"http://example.com");
        assert_eq!(host_bytes(b"https:///a"), b"https:///a");
        assert_eq!(host_bytes(b"https://example.com/a\nb"), b"https://example.com/a\nb");
        assert_eq!(host_bytes(b"http://www./a"), b"www.");
    }
}
