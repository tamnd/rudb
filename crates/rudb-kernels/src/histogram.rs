//! `histogram(x, bins)` and `histogram_exact(x, bins)`, which count rows into bins fixed up front.
//!
//! The bins come from the first row a group sees that is not null, sorted and with duplicates
//! dropped, and every later row of that group is counted against those same bins whatever its own
//! bin list says, which is what the pin does. `histogram` puts a value in the first bin it is not
//! greater than and `histogram_exact` only in a bin equal to it. A value that lands in no bin is
//! counted in one more bin at the end, which the answer carries under a key of its own when it
//! counted anything and the key type has one.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};

use crate::compare::order;
use crate::quantile::{Column, Whole};

/// A group of a binned histogram.
#[derive(Debug, Clone)]
pub(crate) struct Binned {
    /// The bin boundaries in order, once a row has said what they are.
    bins: Option<Vec<Value>>,
    /// A count per bin, and one more for the values no bin took.
    counts: Vec<u64>,
    /// Whether a value has to equal a boundary to be counted in its bin.
    exact: bool,
    /// The type of the answer's keys.
    key: LogicalType,
    /// The bins again as plain numbers, when they are numbers.
    plain: Plain,
}

/// The bins of a [`Binned`] as plain numbers, so that a column of numbers is counted without
/// making a value of each row.
#[derive(Debug, Clone)]
enum Plain {
    Neither,
    Wholes(Whole, Vec<i64>),
    Reals(Vec<f64>),
}

impl Binned {
    /// An empty group of `histogram`, or of `histogram_exact` when `exact`.
    pub(crate) const fn new(exact: bool, key: LogicalType) -> Self {
        Self { bins: None, counts: Vec::new(), exact, key, plain: Plain::Neither }
    }

    /// Counts a value that is not null, with the row's bin list for a group that has none yet.
    pub(crate) fn update(&mut self, value: &Value, bins: Option<&Value>) -> Result<()> {
        if self.bins.is_none() {
            let fixed = boundaries(bins)?;
            self.counts = vec![0; fixed.len() + 1];
            self.plain = plain(&fixed);
            self.bins = Some(fixed);
        }
        let bins = self.bins.as_deref().unwrap_or_default();
        let at = lower_bound(bins, value)?;
        let at = if self.exact && at < bins.len() && !equal(&bins[at], value)? {
            bins.len()
        } else {
            at
        };
        self.counts[at] += 1;
        Ok(())
    }

    /// Counts the row of a column a [`Column`] reads, or says `false` without counting it when the
    /// group has no bins yet or they are not the column's kind of number.
    pub(crate) fn push_column(&mut self, column: Column<'_>, row: usize) -> bool {
        let at = match (&self.plain, column) {
            (Plain::Wholes(kind, bins), Column::Wholes(theirs, numbers)) if *kind == theirs => {
                let n = numbers.at(row);
                let at = bins.partition_point(|&bin| bin < n);
                if self.exact && bins.get(at) != Some(&n) { bins.len() } else { at }
            }
            (Plain::Reals(bins), Column::Reals(reals)) => {
                let real = reals[row];
                let at = bins.partition_point(|&bin| bin < real);
                #[expect(clippy::float_cmp, reason = "an exact bin takes only an equal value")]
                let missed = self.exact && bins.get(at).is_none_or(|&bin| bin != real);
                if missed { bins.len() } else { at }
            }
            _ => return false,
        };
        self.counts[at] += 1;
        true
    }

    /// Adds the counts of another group of the same call, which has to have the same bins.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        let Some(theirs) = &other.bins else { return Ok(()) };
        let Some(bins) = &self.bins else {
            self.bins = Some(theirs.clone());
            self.counts.clone_from(&other.counts);
            return Ok(());
        };
        if bins != theirs {
            return Err(Error::not_implemented(
                "Histogram - cannot combine histograms with different bin boundaries. Bin \
                 boundaries must be the same for all histograms within the same group",
            ));
        }
        for (count, more) in self.counts.iter_mut().zip(&other.counts) {
            *count += more;
        }
        Ok(())
    }

    /// The answer, a map from each bin to its count, or null for a group that counted nothing.
    pub(crate) fn finish(&self) -> Value {
        let Some(bins) = &self.bins else { return Value::Null };
        let mut entries: Vec<(Value, Value)> = bins
            .iter()
            .zip(&self.counts)
            .map(|(bin, &count)| (bin.clone(), Value::UBigInt(count)))
            .collect();
        let others = self.counts.last().copied().unwrap_or(0);
        if others > 0
            && let Some(other) = other_bin(&self.key)
        {
            entries.push((other, Value::UBigInt(others)));
        }
        Value::map(self.key.clone(), LogicalType::UBigInt, entries)
    }
}

/// The bins a row's list asks for, in order and without duplicates.
fn boundaries(list: Option<&Value>) -> Result<Vec<Value>> {
    let Some(Value::List { values, .. }) = list else {
        return Err(Error::binder("Histogram bin list cannot be NULL"));
    };
    if values.iter().any(Value::is_null) {
        return Err(Error::binder("Histogram bin entry cannot be NULL"));
    }
    let mut bins = values.clone();
    let mut failure = None;
    bins.sort_by(|left, right| {
        order(left, right).unwrap_or_else(|error| {
            failure.get_or_insert(error);
            Ordering::Equal
        })
    });
    if let Some(error) = failure {
        return Err(error);
    }
    let mut kept: Vec<Value> = Vec::with_capacity(bins.len());
    for bin in bins {
        match kept.last() {
            Some(last) if order(last, &bin)? == Ordering::Equal => {}
            _ => kept.push(bin),
        }
    }
    Ok(kept)
}

/// The bins as plain numbers, when every one of them is the same kind of whole number or a double.
fn plain(bins: &[Value]) -> Plain {
    if let Some(reals) = bins
        .iter()
        .map(|bin| if let Value::Double(real) = bin { Some(*real) } else { None })
        .collect::<Option<Vec<f64>>>()
    {
        return Plain::Reals(reals);
    }
    let Some((kind, _)) = bins.first().and_then(Whole::of) else { return Plain::Neither };
    bins.iter()
        .map(|bin| Whole::of(bin).filter(|(theirs, _)| *theirs == kind).map(|(_, n)| n))
        .collect::<Option<Vec<i64>>>()
        .map_or(Plain::Neither, |wholes| Plain::Wholes(kind, wholes))
}

/// Whether a bin is less than a value, which for floats is the IEEE comparison the pin makes, so a
/// NaN is less than nothing and lands in the first bin.
fn less(bin: &Value, value: &Value) -> Result<bool> {
    Ok(match (bin, value) {
        (Value::Double(bin), Value::Double(value)) => bin < value,
        (Value::Float(bin), Value::Float(value)) => bin < value,
        _ => order(bin, value)? == Ordering::Less,
    })
}

/// Whether a bin equals a value, which for floats is again the IEEE comparison.
#[expect(clippy::float_cmp, reason = "an exact bin takes only an equal value")]
fn equal(bin: &Value, value: &Value) -> Result<bool> {
    Ok(match (bin, value) {
        (Value::Double(bin), Value::Double(value)) => bin == value,
        (Value::Float(bin), Value::Float(value)) => bin == value,
        _ => order(bin, value)? == Ordering::Equal,
    })
}

/// Where the first bin that is not less than `value` is, or the count of bins when there is none.
fn lower_bound(bins: &[Value], value: &Value) -> Result<usize> {
    let (mut low, mut high) = (0, bins.len());
    while low < high {
        let middle = low + (high - low) / 2;
        if less(&bins[middle], value)? {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

/// The key the values no bin took are counted under, or `None` for a type the pin has no such key
/// for, whose answer leaves those values out.
///
/// The largest value of a whole number or a time, infinity for a date, a timestamp or a float, the
/// empty string or blob, an empty list, and a struct whose fields are all null.
pub(crate) fn other_bin(key: &LogicalType) -> Option<Value> {
    Some(match key {
        LogicalType::TinyInt => Value::TinyInt(i8::MAX),
        LogicalType::SmallInt => Value::SmallInt(i16::MAX),
        LogicalType::Integer => Value::Integer(i32::MAX),
        LogicalType::BigInt => Value::BigInt(i64::MAX),
        LogicalType::HugeInt => Value::HugeInt(i128::MAX),
        LogicalType::UTinyInt => Value::UTinyInt(u8::MAX),
        LogicalType::USmallInt => Value::USmallInt(u16::MAX),
        LogicalType::UInteger => Value::UInteger(u32::MAX),
        LogicalType::UBigInt => Value::UBigInt(u64::MAX),
        LogicalType::UHugeInt => Value::UHugeInt(u128::MAX),
        LogicalType::Time => Value::Time(86_400_000_000),
        LogicalType::Date => Value::Date(i32::MAX),
        LogicalType::Timestamp => Value::Timestamp(i64::MAX),
        LogicalType::TimestampTz => Value::TimestampTz(i64::MAX),
        LogicalType::Float => Value::Float(f32::INFINITY),
        LogicalType::Double => Value::Double(f64::INFINITY),
        LogicalType::Varchar => Value::Varchar(String::new()),
        LogicalType::Blob => Value::Blob(Vec::new()),
        LogicalType::List(element) => {
            Value::List { element: (**element).clone(), values: Vec::new() }
        }
        LogicalType::Struct(fields) => {
            Value::Struct(fields.iter().map(|field| (field.name.clone(), Value::Null)).collect())
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bins(values: &[i32]) -> Value {
        Value::List {
            element: LogicalType::Integer,
            values: values.iter().copied().map(Value::Integer).collect(),
        }
    }

    fn counted(exact: bool, values: &[i32], edges: &[i32]) -> Value {
        let mut state = Binned::new(exact, LogicalType::Integer);
        for &value in values {
            state.update(&Value::Integer(value), Some(&bins(edges))).expect("counts");
        }
        state.finish()
    }

    #[test]
    fn a_binned_histogram_counts_into_the_first_bin_a_value_fits_and_the_rest_at_the_end() {
        let values = [0, 1, 2, 3, 4, 6];
        assert_eq!(
            counted(false, &values, &[5, 3, 1, 3]).to_string(),
            "{1=2, 3=2, 5=1, 2147483647=1}"
        );
        assert_eq!(counted(true, &values, &[1, 3, 5]).to_string(), "{1=1, 3=1, 5=0, 2147483647=4}");
        assert_eq!(counted(false, &[1, 2], &[2]).to_string(), "{2=2}");
        assert_eq!(counted(false, &[], &[2]), Value::Null);
    }

    #[test]
    fn a_binned_histogram_refuses_null_bins_and_to_combine_different_ones() {
        let mut state = Binned::new(false, LogicalType::Integer);
        assert!(state.update(&Value::Integer(1), Some(&Value::Null)).is_err());
        let with_null = Value::List {
            element: LogicalType::Integer,
            values: vec![Value::Integer(1), Value::Null],
        };
        assert!(state.update(&Value::Integer(1), Some(&with_null)).is_err());
        let (mut mine, mut theirs) = (state.clone(), state);
        mine.update(&Value::Integer(1), Some(&bins(&[1]))).expect("counts");
        theirs.update(&Value::Integer(1), Some(&bins(&[2]))).expect("counts");
        assert!(mine.combine(&theirs).is_err());
    }
}
