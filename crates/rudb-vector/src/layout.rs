//! The list of physical layouts, in one place, for the kernels that generate a loop per layout.
//!
//! A kernel that reads values has to do the same thing fifteen times, once per variant of
//! [`Data`](crate::Data), and every kernel in the workspace already writes the body once and lets a
//! local macro repeat it. What none of them shared was the list of layouts the body is repeated
//! over, so the list was written out at ten call sites across four files, and a list written ten
//! times is a list that is wrong in one of them. The failure that causes is not a compile error. It
//! is a layout quietly missing from a kernel's match, falling through to the row at a time path, and
//! the only thing anybody notices is that one column type is thirty times slower than the others.
//!
//! So the lists live here and [`for_each_layout`] hands one to a caller's macro.
//!
//! # How the list is kept honest
//!
//! [`Data::len`](crate::Data::len) is generated from the `all` group and its match has no wildcard
//! arm, so a variant added to `Data` without being added to `all` does not compile. That pins `all`
//! to the enum.
//!
//! The other groups are pinned to `all` by the tests at the bottom of this file, which check that
//! each group is a subset of `all` and that each group plus the layouts it deliberately leaves out
//! is exactly the group above it. The chain runs `narrow` to `exact` to `integer` to `ordered` to
//! `fixed` to `all`, with `signed` and `unsigned` joining at `integer`, so a new fixed width layout
//! that nobody adds to `ordered` fails a test with the name of the layout in the message.
//!
//! It would be better if the groups were derived from one list rather than checked against it, and
//! that is not possible in a declarative macro. Deriving them means filtering the list by a tag, and
//! filtering means comparing one identifier against another, and `macro_rules` cannot compare
//! identifiers. The choice is between a procedural macro crate, which is a build dependency and a
//! second language for six lists, and writing the groups out with a test that says they agree. The
//! test is the cheaper of the two and it fails in the same second the build does.

/// Calls `$callback` with one group of physical layouts.
///
/// Each entry is `(variant, element type, zero)`, where the variant is the
/// [`Data`](crate::Data) variant, the element type is what one value of it is, and the zero is the
/// value that fills a slot whose row is null. A caller that does not need all three ignores the
/// ones it does not need.
///
/// The callback is a macro the caller has already defined, almost always a `macro_rules` inside the
/// function that needs it, so that its body can refer to the function's own locals. It is passed a
/// comma separated list of parenthesised triples and should match
/// `$(($variant:ident, $native:ty, $zero:expr)),+ $(,)?`.
///
/// Anything after the callback name is passed through ahead of the list, one token tree each, for
/// the callers that generate a loop per layout inside another loop per layout and need to hand the
/// inner one what the outer one bound. A caller taking one of those matches it first, as in
/// `($values:expr, $(($variant:ident, $native:ty, $zero:expr)),+ $(,)?)`.
///
/// ```
/// use rudb_vector::{Data, for_each_layout};
///
/// fn widest(data: &Data) -> Option<i128> {
///     macro_rules! biggest {
///         ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
///             match data {
///                 $(Data::$variant(values) => {
///                     values.iter().copied().map(i128::from).max().or(Some($zero))
///                 })+
///                 _ => None,
///             }
///         };
///     }
///     for_each_layout!(narrow, biggest)
/// }
///
/// assert_eq!(widest(&Data::Int32(vec![3, 9, 4].into())), Some(9));
/// assert_eq!(widest(&Data::Float64(vec![3.0].into())), None);
/// ```
///
/// # The groups
///
/// - `all`, every layout that holds values. [`Data::Empty`](crate::Data::Empty) is not in it,
///   because it holds none and has nothing for a loop to read.
/// - `fixed`, every layout whose run is a [`Buffer`](crate::Buffer) of a `Copy` element. These are
///   the ones where a null slot can be filled with a zero and a gather is a copy of fixed width
///   slots rather than a copy of bytes.
/// - `ordered`, the layouts whose SQL order is the derived order of the element type. A float is
///   not in it, because `NaN` orders where SQL says rather than where the hardware says, a string
///   is not in it, because its order is over the bytes a view points at, and an interval is not in
///   it, because three counts that are the same length compare as one number and not as a triple.
/// - `integer`, the ten integer widths.
/// - `signed` and `unsigned`, the five of each that `integer` is made of. Several kernels want one
///   half and not the other, negation being the clearest, since negating an unsigned value is an
///   overflow at every row but zero and a loop for it would be a loop that exists to fail.
/// - `exact`, the integers whose every value fits in an `i128`, so a kernel can widen the lot into
///   one accumulator type. `UInt128` is the one that does not.
/// - `narrow`, the integers narrower than 128 bits. Two things follow from that and both of them
///   are used. The total of a whole vector of them still fits in an `i128`, which is what lets the
///   only overflow check in a sum be the one at the vector boundary rather than one per row, and a
///   run of `i128` narrows into any of them, which is the shape the cast path works in.
/// - `float`, the two IEEE widths.
#[macro_export]
macro_rules! for_each_layout {
    (all, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Bool, bool, false),
            (Int8, i8, 0),
            (Int16, i16, 0),
            (Int32, i32, 0),
            (Int64, i64, 0),
            (Int128, i128, 0),
            (UInt8, u8, 0),
            (UInt16, u16, 0),
            (UInt32, u32, 0),
            (UInt64, u64, 0),
            (UInt128, u128, 0),
            (Float32, f32, 0.0),
            (Float64, f64, 0.0),
            (Interval, (i32, i32, i64), (0, 0, 0)),
            (Varlen, &str, ""),
        }
    };
    (fixed, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Bool, bool, false),
            (Int8, i8, 0),
            (Int16, i16, 0),
            (Int32, i32, 0),
            (Int64, i64, 0),
            (Int128, i128, 0),
            (UInt8, u8, 0),
            (UInt16, u16, 0),
            (UInt32, u32, 0),
            (UInt64, u64, 0),
            (UInt128, u128, 0),
            (Float32, f32, 0.0),
            (Float64, f64, 0.0),
            (Interval, (i32, i32, i64), (0, 0, 0)),
        }
    };
    (ordered, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Bool, bool, false),
            (Int8, i8, 0),
            (Int16, i16, 0),
            (Int32, i32, 0),
            (Int64, i64, 0),
            (Int128, i128, 0),
            (UInt8, u8, 0),
            (UInt16, u16, 0),
            (UInt32, u32, 0),
            (UInt64, u64, 0),
            (UInt128, u128, 0),
        }
    };
    (integer, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Int8, i8, 0),
            (Int16, i16, 0),
            (Int32, i32, 0),
            (Int64, i64, 0),
            (Int128, i128, 0),
            (UInt8, u8, 0),
            (UInt16, u16, 0),
            (UInt32, u32, 0),
            (UInt64, u64, 0),
            (UInt128, u128, 0),
        }
    };
    (signed, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Int8, i8, 0),
            (Int16, i16, 0),
            (Int32, i32, 0),
            (Int64, i64, 0),
            (Int128, i128, 0),
        }
    };
    (unsigned, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (UInt8, u8, 0),
            (UInt16, u16, 0),
            (UInt32, u32, 0),
            (UInt64, u64, 0),
            (UInt128, u128, 0),
        }
    };
    (exact, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Int8, i8, 0),
            (Int16, i16, 0),
            (Int32, i32, 0),
            (Int64, i64, 0),
            (Int128, i128, 0),
            (UInt8, u8, 0),
            (UInt16, u16, 0),
            (UInt32, u32, 0),
            (UInt64, u64, 0),
        }
    };
    (narrow, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Int8, i8, 0),
            (Int16, i16, 0),
            (Int32, i32, 0),
            (Int64, i64, 0),
            (UInt8, u8, 0),
            (UInt16, u16, 0),
            (UInt32, u32, 0),
            (UInt64, u64, 0),
        }
    };
    (float, $callback:ident $(, $extra:tt)*) => {
        $callback! {
            $($extra,)*
            (Float32, f32, 0.0),
            (Float64, f64, 0.0),
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::{Buffer, Data};

    /// The variant names of a group, in the order the group lists them.
    macro_rules! names {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            &[$(stringify!($variant)),+]
        };
    }

    const ALL: &[&str] = for_each_layout!(all, names);
    const FIXED: &[&str] = for_each_layout!(fixed, names);
    const ORDERED: &[&str] = for_each_layout!(ordered, names);
    const INTEGER: &[&str] = for_each_layout!(integer, names);
    const SIGNED: &[&str] = for_each_layout!(signed, names);
    const UNSIGNED: &[&str] = for_each_layout!(unsigned, names);
    const EXACT: &[&str] = for_each_layout!(exact, names);
    const NARROW: &[&str] = for_each_layout!(narrow, names);
    const FLOAT: &[&str] = for_each_layout!(float, names);

    /// The names of a group and some extras, sorted, for comparing one group against another.
    fn sorted(group: &[&str], extra: &[&str]) -> Vec<String> {
        let mut names: Vec<String> =
            group.iter().chain(extra).map(|name| (*name).to_string()).collect();
        names.sort();
        names
    }

    #[test]
    fn no_group_lists_a_layout_twice() {
        for group in [ALL, FIXED, ORDERED, INTEGER, SIGNED, UNSIGNED, EXACT, NARROW, FLOAT] {
            let mut seen = group.to_vec();
            seen.sort_unstable();
            let mut once = seen.clone();
            once.dedup();
            assert_eq!(seen, once, "a group lists the same layout twice");
        }
    }

    #[test]
    fn every_group_is_part_of_the_whole_list() {
        for group in [FIXED, ORDERED, INTEGER, SIGNED, UNSIGNED, EXACT, NARROW, FLOAT] {
            for name in group {
                assert!(ALL.contains(name), "{name} is in a group but not in the all group");
            }
        }
    }

    /// The chain that pins every group to `all`, which the compiler pins to `Data` through
    /// `Data::len`. Each step names the layouts the smaller group leaves out, so a new layout that
    /// only reaches `all` fails here with its own name in the message rather than going missing from
    /// six kernels in silence.
    #[test]
    fn each_group_plus_what_it_leaves_out_is_the_group_above_it() {
        assert_eq!(sorted(ALL, &[]), sorted(FIXED, &["Varlen"]));
        assert_eq!(sorted(FIXED, &[]), sorted(ORDERED, &["Float32", "Float64", "Interval"]));
        assert_eq!(sorted(ORDERED, &[]), sorted(INTEGER, &["Bool"]));
        assert_eq!(sorted(INTEGER, &[]), sorted(SIGNED, UNSIGNED));
        assert_eq!(sorted(INTEGER, &[]), sorted(EXACT, &["UInt128"]));
        assert_eq!(sorted(EXACT, &[]), sorted(NARROW, &["Int128"]));
    }

    /// That the element type and the zero next to a variant are the ones that variant holds. Mostly
    /// a compile time check, since a mismatch would not build, and the assertion is there so that
    /// the built code is also run.
    #[test]
    fn the_element_type_and_the_zero_belong_to_the_variant() {
        macro_rules! built {
            ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
                vec![$(Data::$variant(Buffer::<$native>::from_vec(vec![$zero])),)+]
            };
        }
        let runs = for_each_layout!(fixed, built);
        assert_eq!(runs.len(), FIXED.len());
        for run in runs {
            assert_eq!(run.len(), 1);
        }
    }
}
