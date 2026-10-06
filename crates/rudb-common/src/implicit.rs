//! The pin's implicit casts: which types a value can become without being asked to, and at what
//! cost.
//!
//! This is the pin's `CastRules::ImplicitCast` for the types rudb has. A cost is only ever compared
//! with another one, so the numbers are the pin's own: casting to `BIGINT` is cheaper than casting
//! to `DOUBLE`, which is cheaper than casting to `VARCHAR`, so that an overload that needs the
//! cheaper cast wins.

use crate::types::{Field, LogicalType};

/// What casting a value to `target` costs, by the type it lands in, so that the overload with the
/// cheaper cast wins a tie.
fn target_cost(target: &LogicalType) -> i64 {
    use LogicalType::{
        Array, BigInt, Decimal, Double, HugeInt, Integer, List, Map, Struct, Timestamp,
        TimestampMs, TimestampNs, TimestampS, TimestampTz, Union, Varchar,
    };
    match target {
        BigInt => 101,
        Integer => 102,
        HugeInt => 103,
        Double => 104,
        Decimal { .. } => 105,
        TimestampNs => 119,
        Timestamp => 120,
        TimestampMs => 121,
        TimestampS => 122,
        TimestampTz => 123,
        Varchar => 149,
        Struct(_) | Map(..) | List(_) | Union(_) | Array(..) => 160,
        _ => 110,
    }
}

/// The cost of casting a value of `source` to `target` without being asked to, or `None` when it
/// is not cast implicitly at all.
#[must_use]
pub fn cost(source: &LogicalType, target: &LogicalType) -> Option<i64> {
    use LogicalType::{Array, List, Map, Null, Struct, Union};
    match (source, target) {
        // A null can become anything, and nothing becomes a null.
        (Null, _) => Some(target_cost(target)),
        // A list costs one less than its elements do, so that a list of strings beats a string.
        (List(from), List(to)) => cost(from, to).map(|cost| (cost - 1).max(0)),
        (Array(from, size), Array(to, other)) => {
            let child = if size == other { cost(from, to)? } else { return None };
            Some(if child >= 100 { child - 1 } else { child })
        }
        // An array costs one more to become a list than to become an array, and a list becomes an
        // array of any size, which is checked when the list is.
        (Array(from, _), List(to)) => cost(from, to).map(|cost| cost + 1),
        (List(from), Array(to, _)) => cost(from, to),
        (Union(from), Union(to)) => {
            let mut most = None;
            for member in from {
                let other = to.iter().find(|other| other.name == member.name)?;
                most = most.max(Some(cost(&member.ty, &other.ty).unwrap_or(-1)));
            }
            // When no member casts, the pin falls through to comparing the two types, which are
            // both unions.
            Some(most.map_or(0, |most: i64| most.max(0)))
        }
        (Struct(from), Struct(to)) => structs(from, to),
        // The pin compares two maps by what they are and not by what they hold.
        (Map(..), Map(..)) => Some(0),
        // The same type casts for nothing whatever it holds, so `DECIMAL(4,1)` becomes
        // `DECIMAL(18,3)` and one enum becomes another.
        (from, to) if std::mem::discriminant(from) == std::mem::discriminant(to) => Some(0),
        // A value becomes a union when it becomes one of the members, the cheapest one.
        (_, Union(members)) => member_cost(source, members),
        _ => widens(source, target).then(|| target_cost(target)),
    }
}

/// The cheapest member of a union `source` becomes.
fn member_cost(source: &LogicalType, members: &[Field]) -> Option<i64> {
    members.iter().filter_map(|member| cost(source, &member.ty)).min()
}

/// Two structs, which match field for field by name when both are named and by position when
/// either is not, and cost what their dearest field does.
fn structs(from: &[Field], to: &[Field]) -> Option<i64> {
    if from.len() != to.len() {
        return None;
    }
    if from.is_empty() {
        return Some(0);
    }
    let mut most = -1;
    if Field::unnamed(from) || Field::unnamed(to) {
        for (source, target) in from.iter().zip(to) {
            most = most.max(cost(&source.ty, &target.ty)?);
        }
        return Some(most);
    }
    let named = |field: &&Field, name: &str| field.name.eq_ignore_ascii_case(name);
    if to
        .iter()
        .enumerate()
        .any(|(at, field)| to[..at].iter().any(|other| named(&other, &field.name)))
    {
        return None;
    }
    let mut left: Vec<&Field> = to.iter().collect();
    for source in from {
        let at = left.iter().position(|target| named(target, &source.name))?;
        most = most.max(cost(&source.ty, &left.remove(at).ty)?);
    }
    Some(most)
}

/// Whether the pin widens a value of `source` into `target` on its own, for two types that are not
/// the same and not nested.
fn widens(source: &LogicalType, target: &LogicalType) -> bool {
    use LogicalType::{
        BigInt, BigNum, Date, Decimal, Double, Enum, Float, HugeInt, Integer, Numeric, SmallInt, Timestamp,
        TimestampMs, TimestampNs, TimestampS, TimestampTz, TinyInt, UBigInt, UHugeInt, UInteger,
        USmallInt, UTinyInt, Varchar,
    };
    // Every integer and a `FLOAT` go into a number of any size, and that goes into a double, which
    // is how the pin multiplies one.
    if (source.is_integer() || *source == Float) && *target == BigNum {
        return true;
    }
    // An integer and a decimal go into the `numeric` of PostgreSQL, and that goes into a double.
    if (source.is_integer() || matches!(source, Decimal { .. })) && *target == Numeric {
        return true;
    }
    match source {
        TinyInt => matches!(
            target,
            SmallInt | Integer | BigInt | HugeInt | Float | Double | Decimal { .. }
        ),
        SmallInt => matches!(target, Integer | BigInt | HugeInt | Float | Double | Decimal { .. }),
        Integer => matches!(target, BigInt | HugeInt | Float | Double | Decimal { .. }),
        BigInt | HugeInt => matches!(target, HugeInt | Float | Double | Decimal { .. }),
        UTinyInt => matches!(
            target,
            USmallInt
                | UInteger
                | UBigInt
                | SmallInt
                | Integer
                | BigInt
                | HugeInt
                | UHugeInt
                | Float
                | Double
                | Decimal { .. }
        ),
        USmallInt => matches!(
            target,
            UInteger
                | UBigInt
                | Integer
                | BigInt
                | HugeInt
                | UHugeInt
                | Float
                | Double
                | Decimal { .. }
        ),
        UInteger => matches!(
            target,
            UBigInt | BigInt | UHugeInt | HugeInt | Float | Double | Decimal { .. }
        ),
        UBigInt => matches!(target, UHugeInt | HugeInt | Float | Double | Decimal { .. }),
        UHugeInt => matches!(target, Float | Double | Decimal { .. }),
        Float | Decimal { .. } => matches!(target, Float | Double),
        Date => matches!(target, Timestamp | TimestampTz | TimestampMs | TimestampNs | TimestampS),
        Enum(_) => matches!(target, Varchar),
        BigNum | Numeric => matches!(target, Double),
        TimestampS => matches!(target, Timestamp | TimestampMs | TimestampNs),
        TimestampMs => matches!(target, Timestamp | TimestampNs),
        TimestampNs => matches!(target, Timestamp),
        Timestamp => matches!(target, TimestampNs | TimestampTz),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LogicalType::{BigInt, Date, Double, Integer, Null, Timestamp, TimestampTz, Varchar};

    #[test]
    fn a_number_widens_and_never_narrows() {
        assert_eq!(cost(&Integer, &BigInt), Some(101));
        assert_eq!(cost(&BigInt, &Integer), None);
        assert_eq!(cost(&Integer, &Varchar), None);
        assert_eq!(cost(&Double, &LogicalType::Float), None);
    }

    #[test]
    fn a_null_becomes_anything_and_nothing_becomes_a_null() {
        assert_eq!(cost(&Null, &Integer), Some(102));
        assert_eq!(cost(&Integer, &Null), None);
    }

    #[test]
    fn a_date_becomes_any_timestamp_and_a_zoned_one_stays_zoned() {
        assert_eq!(cost(&Date, &TimestampTz), Some(123));
        assert_eq!(cost(&TimestampTz, &Timestamp), None);
    }
}
