//! One schema out of several files.
//!
//! `read_csv('data/*.csv')` is one stream of rows and a stream has one schema, so the files a
//! pattern matched have to agree on one. A Parquet file states its schema and the first file's is
//! taken as the answer, but a CSV file states nothing, so the binder sniffs the first few files and
//! combines the answers, which is the pin's `CSVSchemaDiscovery`. That is not an optimisation
//! choice, it is a correctness one: a directory of daily exports where one day's file happens to
//! hold whole numbers in a column that is otherwise decimal would otherwise come out BIGINT or
//! DOUBLE depending on which day sorted first.
//!
//! [`merge`] is how two answers combine. It goes by position and not by name, and a file the
//! sniffer found no rows in has no say, so a file that is only a header does not turn every column
//! into text. The names are the first file with a row in it. A file read after that is held to them
//! by name, and [`mismatch`] is the complaint when it does not have one or holds a type that does
//! not go into it, which [`can_cast`] decides.
//!
//! `union_by_name` is the other way, where every file is sniffed alone and the columns are matched
//! by name. That combines types the way a `UNION` does, so it is the binder's to do.

use rudb_common::{Error, Field, LogicalType};

/// Whether values the sniffer read as `from` go into a column of `to` without a loss the pin
/// minds, which is its `CSVSchema::CanWeCastIt`.
///
/// Anything goes into text, and the integers go up into the wider integers and the floats. The
/// rest only go into themselves, so a date does not go into a timestamp.
#[must_use]
pub fn can_cast(from: &LogicalType, to: &LogicalType) -> bool {
    use LogicalType::{BigInt, BigNum, Double, Float, HugeInt, Integer, Null, SmallInt, TinyInt};
    if *to == LogicalType::Varchar || from == to {
        return true;
    }
    let decimal = matches!(to, LogicalType::Decimal { .. });
    let wide = |to: &LogicalType| matches!(to, HugeInt | BigNum | Float | Double) || decimal;
    match from {
        Null => true,
        TinyInt => matches!(to, SmallInt | Integer | BigInt) || wide(to),
        SmallInt => matches!(to, Integer | BigInt) || wide(to),
        Integer => *to == BigInt || wide(to),
        BigInt => wide(to),
        HugeInt | LogicalType::UHugeInt => matches!(to, BigNum | Double),
        BigNum | Float => *to == Double,
        _ => false,
    }
}

/// Folds a later file's sniffed columns into the ones settled so far, column by column by position,
/// which is the pin's `CSVSchema::MergeSchemas`.
///
/// Where one type goes into the other the column takes the wider one, and where neither does it
/// takes the first of `BOOLEAN`, `BIGINT`, `HUGEINT`, `BIGNUM`, `DOUBLE` and `VARCHAR` both go
/// into. A file with more columns adds nothing and a file with fewer leaves the rest as they were.
pub fn merge(fields: &mut [Field], other: &[Field]) {
    const PARENTS: [LogicalType; 6] = [
        LogicalType::Boolean,
        LogicalType::BigInt,
        LogicalType::HugeInt,
        LogicalType::BigNum,
        LogicalType::Double,
        LogicalType::Varchar,
    ];
    for (field, other) in fields.iter_mut().zip(other) {
        if field.ty == other.ty {
            continue;
        }
        if can_cast(&field.ty, &other.ty) {
            field.ty = other.ty.clone();
        } else if !can_cast(&other.ty, &field.ty)
            && let Some(parent) = PARENTS
                .iter()
                .find(|parent| can_cast(&field.ty, parent) && can_cast(&other.ty, parent))
        {
            field.ty = parent.clone();
        }
    }
}

/// What a later file disagrees with the settled columns about, or nothing when it agrees.
///
/// A column it does not have and a column whose type does not go into the settled one are both
/// named, every one of them, in the order the settled columns are in.
#[must_use]
pub fn disagreements(settled: &[Field], held: &[Field]) -> Vec<String> {
    let mut found = Vec::new();
    for field in settled {
        match held.iter().find(|column| column.name == field.name) {
            None => found.push(format!("Column with name: \"{}\" is missing", field.name)),
            Some(column) if !can_cast(&column.ty, &field.ty) => found.push(format!(
                "Column with name: \"{}\" is expected to have type: {} But has type: {}",
                field.name, field.ty, column.ty
            )),
            Some(_) => {}
        }
    }
    found
}

/// DuckDB's message for a file of a read that does not agree with the file the names came from,
/// given what [`disagreements`] found.
///
/// The trailing space after `Potential Fixes` is the binary's and is kept, because a compatibility
/// test that compares output compares all of it and a difference that is invisible on a terminal is
/// the worst kind to go looking for later.
#[must_use]
pub fn mismatch(main: &str, current: &str, found: &[String]) -> Error {
    let mut message = format!(
        "Schema mismatch between globbed files.\nMain file schema: {main}\nCurrent file: \
         {current}\n"
    );
    for line in found {
        message.push_str(line);
        message.push('\n');
    }
    message.push_str(
        "Potential Fixes \n* Consider setting union_by_name=true.\n* Consider setting \
         files_to_sniff to a higher value (e.g., files_to_sniff = -1)\n",
    );
    Error::invalid_input(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, ty: LogicalType) -> Field {
        Field::new(name, ty)
    }

    fn merged(one: LogicalType, other: LogicalType) -> LogicalType {
        let mut fields = vec![named("a", one)];
        merge(&mut fields, &[named("a", other)]);
        fields.remove(0).ty
    }

    #[test]
    fn the_integers_widen_and_everything_else_meets_in_text() {
        assert_eq!(merged(LogicalType::BigInt, LogicalType::BigInt), LogicalType::BigInt);
        assert_eq!(merged(LogicalType::BigInt, LogicalType::Double), LogicalType::Double);
        assert_eq!(merged(LogicalType::Double, LogicalType::BigInt), LogicalType::Double);
        assert_eq!(merged(LogicalType::Boolean, LogicalType::BigInt), LogicalType::Varchar);
        assert_eq!(merged(LogicalType::Date, LogicalType::BigInt), LogicalType::Varchar);
        // Not TIMESTAMP, which is what a lattice would say and is not what the binary does.
        assert_eq!(merged(LogicalType::Date, LogicalType::Timestamp), LogicalType::Varchar);
        assert_eq!(merged(LogicalType::Varchar, LogicalType::BigInt), LogicalType::Varchar);
    }

    #[test]
    fn the_columns_are_merged_by_position() {
        let mut fields = vec![named("a", LogicalType::BigInt), named("b", LogicalType::Varchar)];
        merge(&mut fields, &[named("b", LogicalType::Double), named("c", LogicalType::BigInt)]);
        assert_eq!(fields[0], named("a", LogicalType::Double));
        assert_eq!(fields[1], named("b", LogicalType::Varchar));
    }

    #[test]
    fn a_later_file_is_held_to_the_names_and_the_types_it_goes_into() {
        let settled = [named("a", LogicalType::BigInt), named("b", LogicalType::Varchar)];
        let held = [named("b", LogicalType::Double), named("c", LogicalType::BigInt)];
        assert_eq!(disagreements(&settled, &held), ["Column with name: \"a\" is missing"]);
        let held = [named("a", LogicalType::Double), named("b", LogicalType::BigInt)];
        let found = disagreements(&settled, &held);
        assert_eq!(
            found,
            ["Column with name: \"a\" is expected to have type: BIGINT But has type: DOUBLE"]
        );
        let error = mismatch("one.csv", "two.csv", &found);
        assert!(error.message().starts_with("Schema mismatch between globbed files."), "{error}");
        assert!(error.message().contains("Main file schema: one.csv\n"), "{error}");
        assert!(error.message().ends_with("(e.g., files_to_sniff = -1)\n"), "{error}");
    }
}
