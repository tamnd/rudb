//! What `duckdb_types()` says about each type name this engine knows.
//!
//! One entry per name, and one row per entry per modifier signature, which is why 73 names produce 93
//! rows. The list, the oids, the modifier signatures and the row order were all read off the pinned
//! binary rather than worked out from first principles, because every one of them turned out to have
//! something in it that reading the type system would not have told you.
//!
//! # The table lists the types this engine has
//!
//! The pinned binary returns 104 rows in the `memory` schema and this returns 93. The eleven that are
//! not here are ten names for types rudb does not have at all, `array`, `bignum`, `enum`,
//! `geometry`, `timestamptz_ns`, `time_ns`, `tuple`, `type`, `variant` and `varint`, plus `geometry`
//! a second time for its `crs` modifier. A catalog table that listed a type you cannot make a value
//! of would be a table that lies, and the point of this one is that a client can read it to find out
//! what the engine supports. The names come back when the types do.
//!
//! `list` is here even though `NULL::LIST(INTEGER)` is a parser error in both engines. The name is a
//! catalog entry rather than something a cast can spell, the spelling that works is `INTEGER[]`, and
//! rudb has had a list vector since #302, so the row belongs here for the same reason it is there.
//!
//! # A name is not a type and the modifiers are per name
//!
//! `timestamp` takes a `precision` modifier and `timestamp_us` does not, and they are the same type.
//! `varchar` takes `length` and `collation` and `blob` takes neither, and both are variable length
//! bytes. So the signatures are stored per name and not derived from the type, which was the first
//! guess and is wrong six ways.
//!
//! What is derived is the size and the category, because those really are properties of the type.
//! `type_size` comes from [`rudb_common::PhysicalType::size`], which is where the three
//! divergences from the pin are written down, and the category is the function below.
//!
//! # `type_oid` is on one row out of however many a type has
//!
//! The oids are `LogicalTypeId`, which is a stable enumeration in someone else's public header, so
//! reproducing them is meaningful in a way that reproducing a catalog's allocation counter is not.
//! A type has one oid and several names, and the pin puts the oid on the alphabetically first of
//! those names, on that name's bare signature row, and leaves it null everywhere else. So `bigint`
//! carries 14 and `int8`, `int64`, `long` and `oid` carry nothing, and `bpchar` carries 25 while
//! `varchar` carries nothing. That is measured, not guessed, and `the_oid_sits_on_the_first_name_of_its_type`
//! re-derives it from the entries so a new alias cannot silently take an oid off the name that had it.
//!
//! # Row order
//!
//! Reproduced, unlike `duckdb_keywords()`, because it is a rule rather than an implementation detail.
//! Names sort case insensitively with `_` ranked after the letters, so `timestamptz_ns` comes before
//! `timestamp_ms` and `timetz` before `time_ns`, and a space sorts first, so `time with time zone`
//! comes directly after `time`. Within one name the signatures are in declaration order, which is why
//! `bpchar` is bare then `length` then `collation` rather than in alphabetical order.

use rudb_common::{Field, LogicalType};

/// One modifier signature: the parameter names and the type each one takes.
pub type Signature = &'static [(&'static str, &'static str)];

/// One type name in the catalog, and everything `duckdb_types()` says about it.
#[derive(Debug, Clone, Copy)]
pub struct TypeEntry {
    /// The name, as the pin spells it, which is lower case for everything rudb has.
    pub name: &'static str,
    /// The canonical type this name means, as the pin writes it in the `logical_type` column.
    pub logical_type: &'static str,
    /// The `LogicalTypeId` of the type, on the one name that carries it and `None` on the rest.
    pub oid: Option<i64>,
    /// The type the trailing variadic argument takes, for the names that have one.
    pub varargs: Option<&'static str>,
    /// One per row this name produces, in the order the pin returns them.
    pub signatures: &'static [Signature],
}

/// No modifiers, which is what most names have and all a few of them have.
const BARE: &[Signature] = &[&[]];

/// The three a string name takes, in the pin's order.
const STRING: &[Signature] = &[&[], &[("length", "BIGINT")], &[("collation", "VARCHAR")]];

/// Bare or a length, which is the bit string pair.
const LENGTH: &[Signature] = &[&[], &[("length", "BIGINT")]];

/// Bare or a width and a scale, which is the decimal pair.
const WIDTH_SCALE: &[Signature] = &[&[], &[("width", "UTINYINT"), ("scale", "UTINYINT")]];

/// Bare or a precision, which is what the two names that take one have.
const PRECISION: &[Signature] = &[&[], &[("precision", "UTINYINT")]];

/// Every type name, in the order `duckdb_types()` returns them.
///
/// See the module documentation for why the order is what it is, why the list is shorter than the
/// pin's, and why the oid is on the name it is on.
pub static TYPE_NAMES: &[TypeEntry] = &[
    entry("bigint", "BIGINT", Some(14)),
    entry("binary", "BLOB", Some(26)),
    TypeEntry { signatures: LENGTH, oid: Some(36), ..entry("bit", "BIT", None) },
    TypeEntry { signatures: LENGTH, ..entry("bitstring", "BIT", None) },
    entry("blob", "BLOB", None),
    entry("bool", "BOOLEAN", Some(10)),
    entry("boolean", "BOOLEAN", None),
    TypeEntry { signatures: STRING, oid: Some(25), ..entry("bpchar", "VARCHAR", None) },
    entry("bytea", "BLOB", None),
    TypeEntry { signatures: STRING, ..entry("char", "VARCHAR", None) },
    entry("date", "DATE", Some(15)),
    TypeEntry { signatures: PRECISION, oid: Some(19), ..entry("datetime", "TIMESTAMP", None) },
    TypeEntry { signatures: WIDTH_SCALE, oid: Some(21), ..entry("dec", "DECIMAL", None) },
    TypeEntry { signatures: WIDTH_SCALE, ..entry("decimal", "DECIMAL", None) },
    entry("double", "DOUBLE", Some(23)),
    entry("float", "FLOAT", Some(22)),
    entry("float4", "FLOAT", None),
    entry("float8", "DOUBLE", None),
    entry("guid", "UUID", Some(54)),
    entry("hugeint", "HUGEINT", Some(50)),
    entry("int", "INTEGER", Some(13)),
    entry("int1", "TINYINT", Some(11)),
    entry("int128", "HUGEINT", None),
    entry("int16", "SMALLINT", Some(12)),
    entry("int2", "SMALLINT", None),
    entry("int32", "INTEGER", None),
    entry("int4", "INTEGER", None),
    entry("int64", "BIGINT", None),
    entry("int8", "BIGINT", None),
    entry("integer", "INTEGER", None),
    entry("integral", "INTEGER", None),
    TypeEntry { signatures: PRECISION, oid: Some(27), ..entry("interval", "INTERVAL", None) },
    TypeEntry {
        signatures: &[&[("child", "TYPE")]],
        oid: Some(101),
        ..entry("list", "LIST", None)
    },
    entry("logical", "BOOLEAN", None),
    entry("long", "BIGINT", None),
    TypeEntry {
        signatures: &[&[("key", "TYPE"), ("value", "TYPE")]],
        oid: Some(102),
        ..entry("map", "MAP", None)
    },
    entry("null", "NULL", Some(1)),
    TypeEntry { signatures: WIDTH_SCALE, ..entry("numeric", "DECIMAL", None) },
    TypeEntry { signatures: STRING, ..entry("nvarchar", "VARCHAR", None) },
    entry("oid", "BIGINT", None),
    entry("real", "FLOAT", None),
    TypeEntry { varargs: Some("TYPE"), oid: Some(100), ..entry("row", "STRUCT", None) },
    entry("short", "SMALLINT", None),
    entry("signed", "INTEGER", None),
    entry("smallint", "SMALLINT", None),
    TypeEntry { signatures: STRING, ..entry("string", "VARCHAR", None) },
    TypeEntry { varargs: Some("TYPE"), ..entry("struct", "STRUCT", None) },
    TypeEntry { signatures: STRING, ..entry("text", "VARCHAR", None) },
    entry("time", "TIME", Some(16)),
    entry("time with time zone", "TIME WITH TIME ZONE", Some(34)),
    TypeEntry { signatures: PRECISION, ..entry("timestamp", "TIMESTAMP", None) },
    entry("timestamp with time zone", "TIMESTAMP WITH TIME ZONE", Some(32)),
    entry("timestamptz", "TIMESTAMP WITH TIME ZONE", None),
    entry("timestamp_ms", "TIMESTAMP_MS", Some(18)),
    entry("timestamp_ns", "TIMESTAMP_NS", Some(20)),
    entry("timestamp_s", "TIMESTAMP_S", Some(17)),
    entry("timestamp_us", "TIMESTAMP", None),
    entry("timetz", "TIME WITH TIME ZONE", None),
    entry("tinyint", "TINYINT", None),
    entry("ubigint", "UBIGINT", Some(31)),
    entry("uhugeint", "UHUGEINT", Some(49)),
    entry("uint128", "UHUGEINT", None),
    entry("uint16", "USMALLINT", Some(29)),
    entry("uint32", "UINTEGER", Some(30)),
    entry("uint64", "UBIGINT", None),
    entry("uint8", "UTINYINT", Some(28)),
    entry("uinteger", "UINTEGER", None),
    TypeEntry { varargs: Some("TYPE"), oid: Some(107), ..entry("union", "UNION", None) },
    entry("usmallint", "USMALLINT", None),
    entry("utinyint", "UTINYINT", None),
    entry("uuid", "UUID", None),
    entry("varbinary", "BLOB", None),
    TypeEntry { signatures: STRING, ..entry("varchar", "VARCHAR", None) },
];

/// One bare name, which is the shape most of the table is, so the rest can be written as a change to
/// it rather than as seventy repetitions of the same four fields.
const fn entry(name: &'static str, logical_type: &'static str, oid: Option<i64>) -> TypeEntry {
    TypeEntry { name, logical_type, oid, varargs: None, signatures: BARE }
}

/// The columns `duckdb_types()` produces, which is DuckDB's seventeen.
#[must_use]
pub fn type_fields() -> Vec<Field> {
    vec![
        Field::new("database_name", LogicalType::Varchar),
        Field::new("database_oid", LogicalType::BigInt),
        Field::new("schema_name", LogicalType::Varchar),
        Field::new("schema_oid", LogicalType::BigInt),
        Field::new("type_oid", LogicalType::BigInt),
        Field::new("type_name", LogicalType::Varchar),
        Field::new("type_size", LogicalType::BigInt),
        Field::new("logical_type", LogicalType::Varchar),
        Field::new("type_category", LogicalType::Varchar),
        Field::new("comment", LogicalType::Varchar),
        Field::new("tags", LogicalType::map(LogicalType::Varchar, LogicalType::Varchar)),
        Field::new("internal", LogicalType::Boolean),
        Field::new("extension_name", LogicalType::Varchar),
        Field::new("labels", LogicalType::list(LogicalType::Varchar)),
        Field::new("parameters", LogicalType::list(LogicalType::Varchar)),
        Field::new("parameter_types", LogicalType::list(LogicalType::Varchar)),
        Field::new("varargs", LogicalType::Varchar),
    ]
}

/// The representative type a canonical name stands for, and `None` for a name that is a family
/// rather than a type.
///
/// A `DECIMAL` has no one width and a `LIST` has no one element, so the argument is whatever makes
/// the answer to the two questions this is asked right: the layout and the category. A decimal of
/// any width is `NUMERIC` and is stored in an integer whose size depends on the width, which is why
/// the size is reported null and the category is not.
#[must_use]
pub fn representative(logical_type: &str) -> Option<LogicalType> {
    Some(match logical_type {
        "NULL" => LogicalType::Null,
        "BOOLEAN" => LogicalType::Boolean,
        "TINYINT" => LogicalType::TinyInt,
        "SMALLINT" => LogicalType::SmallInt,
        "INTEGER" => LogicalType::Integer,
        "BIGINT" => LogicalType::BigInt,
        "HUGEINT" => LogicalType::HugeInt,
        "UTINYINT" => LogicalType::UTinyInt,
        "USMALLINT" => LogicalType::USmallInt,
        "UINTEGER" => LogicalType::UInteger,
        "UBIGINT" => LogicalType::UBigInt,
        "UHUGEINT" => LogicalType::UHugeInt,
        "FLOAT" => LogicalType::Float,
        "DOUBLE" => LogicalType::Double,
        "DECIMAL" => LogicalType::Decimal { width: 18, scale: 3 },
        "VARCHAR" => LogicalType::Varchar,
        "BLOB" => LogicalType::Blob,
        "BIT" => LogicalType::Bit,
        "UUID" => LogicalType::Uuid,
        "DATE" => LogicalType::Date,
        "TIME" => LogicalType::Time,
        "TIME WITH TIME ZONE" => LogicalType::TimeTz,
        "TIMESTAMP" => LogicalType::Timestamp,
        "TIMESTAMP_S" => LogicalType::TimestampS,
        "TIMESTAMP_MS" => LogicalType::TimestampMs,
        "TIMESTAMP_NS" => LogicalType::TimestampNs,
        "TIMESTAMP WITH TIME ZONE" => LogicalType::TimestampTz,
        "INTERVAL" => LogicalType::Interval,
        "LIST" => LogicalType::list(LogicalType::Integer),
        "MAP" => LogicalType::map(LogicalType::Varchar, LogicalType::Varchar),
        "STRUCT" => LogicalType::Struct(Vec::new()),
        "UNION" => LogicalType::Union(Vec::new()),
        _ => return None,
    })
}

/// The `LogicalTypeId` of a canonical type name, and `None` for one this catalog does not carry.
///
/// The same number [`TypeEntry::oid`] holds, read the other way round. That column puts the oid on
/// the alphabetically first name of a type and leaves it null on the aliases, because that is what
/// the pin does, so finding a type's oid means scanning for the one entry that has it rather than
/// looking up a name. `duckdb_columns()` reports this as `data_type_id` and does not care which name
/// somebody wrote the column with, so `INTEGER` and `int4` both come out as 13.
#[must_use]
pub fn type_oid(logical_type: &str) -> Option<i64> {
    TYPE_NAMES
        .iter()
        .find(|entry| entry.logical_type == logical_type && entry.oid.is_some())
        .and_then(|entry| entry.oid)
}

/// How many bytes one value of this name takes, and `None` for the one name where it depends.
///
/// A decimal is stored in the narrowest integer that holds its width, so there is no answer until
/// somebody says how wide. The pinned binary reports null for the same reason.
#[must_use]
pub fn type_size(logical_type: &str) -> Option<i64> {
    if logical_type == "DECIMAL" {
        return None;
    }
    let ty = representative(logical_type)?;
    i64::try_from(ty.physical().size()).ok()
}

/// Which of DuckDB's categories a type is in, and `None` for the ones it puts in none.
///
/// Six categories and a gap. `BIT`, `BLOB`, `UUID` and the null type are in no category at all,
/// which is not an oversight anybody can fix from here, it is what the pin reports and this table is
/// checked against the pin.
#[must_use]
pub fn type_category(logical_type: &str) -> Option<&'static str> {
    let ty = representative(logical_type)?;
    Some(match ty {
        LogicalType::Boolean => "BOOLEAN",
        LogicalType::Varchar => "STRING",
        LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::BigInt
        | LogicalType::HugeInt
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::UBigInt
        | LogicalType::UHugeInt
        | LogicalType::Float
        | LogicalType::Double
        | LogicalType::Decimal { .. } => "NUMERIC",
        LogicalType::Date
        | LogicalType::Time
        | LogicalType::TimeTz
        | LogicalType::Timestamp
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs
        | LogicalType::TimestampTz
        | LogicalType::Interval => "DATETIME",
        LogicalType::List(_)
        | LogicalType::Array(_, _)
        | LogicalType::Map(_, _)
        | LogicalType::Struct(_)
        | LogicalType::Union(_) => "COMPOSITE",
        _ => return None,
    })
}

/// The sort key a type name is ordered by, which is not the name.
///
/// Case insensitive, and `_` ranked after the letters rather than before them, which is how the pin
/// puts `timestamptz_ns` before `timestamp_ms` and `timetz` before `time_ns`. A space is left alone
/// and so sorts first, which is how `time with time zone` lands directly after `time`.
#[must_use]
pub fn sort_key(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "{")
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::{TYPE_NAMES, sort_key, type_category, type_fields, type_size};

    #[test]
    fn the_table_is_the_shape_the_pin_returns() {
        let rows: usize = TYPE_NAMES.iter().map(|entry| entry.signatures.len()).sum();
        assert_eq!(TYPE_NAMES.len(), 73, "names");
        assert_eq!(rows, 93, "rows, which is the pin's 104 less the eleven for types we lack");
        assert_eq!(type_fields().len(), 17);
    }

    /// The rule the pin follows, re-derived here rather than trusted, so that adding an alias that
    /// sorts before the name currently carrying an oid fails instead of producing two oids or none.
    #[test]
    fn the_oid_sits_on_the_first_name_of_its_type() {
        for entry in TYPE_NAMES {
            let first = TYPE_NAMES
                .iter()
                .filter(|other| other.logical_type == entry.logical_type)
                .min_by_key(|other| sort_key(other.name))
                .expect("at least itself");
            let expected = entry.name == first.name;
            assert_eq!(
                entry.oid.is_some(),
                expected,
                "{} carries an oid and {} is the first name of {}",
                entry.name,
                first.name,
                entry.logical_type
            );
        }
        // Every type has exactly one oid and no two types share one.
        let mut oids: Vec<i64> = TYPE_NAMES.iter().filter_map(|entry| entry.oid).collect();
        oids.sort_unstable();
        let total = oids.len();
        oids.dedup();
        assert_eq!(oids.len(), total, "two names claim the same oid");
        assert_eq!(total, 32, "one oid per type this engine has");
    }

    #[test]
    fn the_names_are_in_the_order_the_pin_returns_them() {
        let mut sorted: Vec<&str> = TYPE_NAMES.iter().map(|entry| entry.name).collect();
        sorted.sort_by_key(|name| sort_key(name));
        let listed: Vec<&str> = TYPE_NAMES.iter().map(|entry| entry.name).collect();
        assert_eq!(listed, sorted);
        // The three pairs that say the collation is not the byte order.
        assert!(sort_key("timestamptz_ns") < sort_key("timestamp_ms"));
        assert!(sort_key("timetz") < sort_key("time_ns"));
        assert!(sort_key("time with time zone") < sort_key("timestamp"));
    }

    /// Every name in this table has to be a name the type parser accepts, or the table is advertising
    /// something a query cannot use. `list` is the one exception and the module says why.
    #[test]
    fn every_name_here_is_a_name_a_cast_can_spell() {
        for entry in TYPE_NAMES {
            if entry.name == "list" {
                assert!(LogicalType::parse("list").is_err(), "the pin refuses this too");
                continue;
            }
            let spelled = match entry.logical_type {
                "DECIMAL" => format!("{}(9, 2)", entry.name),
                "MAP" => format!("{}(VARCHAR, VARCHAR)", entry.name),
                "STRUCT" | "UNION" => format!("{}(a INTEGER)", entry.name),
                _ => entry.name.to_string(),
            };
            let parsed = LogicalType::parse(&spelled)
                .unwrap_or_else(|e| panic!("{} does not parse: {e}", entry.name));
            let canonical = entry.logical_type.to_string();
            // The null type prints with quotes round it, which is DuckDB's spelling in a `typeof` and
            // not its spelling in this column, so the two really do differ by the quotes and nothing
            // else. The parser accepts both, which is why a quoted name is a type name at all.
            let got = parsed.to_string().replace('"', "");
            assert!(
                got == canonical || got.starts_with(&canonical),
                "{} parses to {got} and the table says {canonical}",
                entry.name
            );
        }
    }

    /// The three numbers that are this engine's layout rather than the pin's, kept as an assertion so
    /// that a change to either one is a decision somebody makes.
    #[test]
    fn the_sizes_are_this_engines_and_three_of_them_differ_from_the_pin() {
        assert_eq!(type_size("INTEGER"), Some(4));
        assert_eq!(type_size("VARCHAR"), Some(16));
        assert_eq!(type_size("HUGEINT"), Some(16));
        assert_eq!(type_size("DECIMAL"), None, "the width decides, so there is no one answer");
        assert_eq!(type_size("STRUCT"), Some(0), "the parent holds nothing of its own");
        // Two u32 here and two u64 there.
        assert_eq!(type_size("LIST"), Some(8), "the pin says 16");
        assert_eq!(type_size("MAP"), Some(8), "the pin says 16");
        // Nothing stored at all here, an INT32 body there.
        assert_eq!(type_size("NULL"), Some(0), "the pin says 4");
    }

    #[test]
    fn four_types_are_in_no_category_and_that_is_the_pins_answer() {
        assert_eq!(type_category("BIGINT"), Some("NUMERIC"));
        assert_eq!(type_category("DECIMAL"), Some("NUMERIC"));
        assert_eq!(type_category("VARCHAR"), Some("STRING"));
        assert_eq!(type_category("BOOLEAN"), Some("BOOLEAN"));
        assert_eq!(type_category("INTERVAL"), Some("DATETIME"));
        assert_eq!(type_category("MAP"), Some("COMPOSITE"));
        for uncategorised in ["NULL", "BIT", "BLOB", "UUID"] {
            assert_eq!(type_category(uncategorised), None, "{uncategorised}");
        }
    }

    /// The signatures are per name and not per type, which is the thing about this table that looks
    /// like a mistake until you check it against the pin.
    #[test]
    fn two_names_for_one_type_can_take_different_modifiers() {
        let of = |name: &str| {
            TYPE_NAMES.iter().find(|entry| entry.name == name).expect("a name in the table")
        };
        assert_eq!(of("timestamp").signatures.len(), 2, "timestamp takes a precision");
        assert_eq!(of("timestamp_us").signatures.len(), 1, "the same type, and it does not");
        assert_eq!(of("varchar").signatures.len(), 3);
        assert_eq!(of("blob").signatures.len(), 1);
        // Declaration order and not alphabetical, which is what the pin returns.
        assert_eq!(of("bpchar").signatures[1], [("length", "BIGINT")]);
        assert_eq!(of("bpchar").signatures[2], [("collation", "VARCHAR")]);
    }
}
