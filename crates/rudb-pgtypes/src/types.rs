//! The type of a PostgreSQL value as a client sees it, and the facts of each built-in type.

use std::borrow::Cow;

use rudb_common::SqlState;

use crate::error::TypeError;
use crate::generated::oids::TYPES;
use crate::keywords::quote_identifier;
use crate::oid;
use crate::typmod::{
    INTERVAL_FULL_PRECISION, INTERVAL_FULL_RANGE, IntervalField, char_length,
    interval_precision_range, interval_range, numeric_precision_scale,
};

/// The object ID of a type, as in `pg_type.oid`.
pub type Oid = u32;

/// The PostgreSQL type of a value: the OID that `RowDescription` and `ParameterDescription` send,
/// and the typmod. A typmod of -1 means that the type has no modifier. Document 06 section 6.5 of
/// the PostgreSQL notes says why rudb carries this beside its own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PgType {
    pub oid: Oid,
    pub typmod: i32,
}

impl PgType {
    /// The type with no typmod.
    pub const fn new(oid: Oid) -> PgType {
        PgType { oid, typmod: -1 }
    }

    pub const fn with_typmod(oid: Oid, typmod: i32) -> PgType {
        PgType { oid, typmod }
    }

    /// The row of the type in `pg_type`, if it is a built-in type.
    pub fn info(self) -> Option<&'static TypeInfo> {
        TypeInfo::get(self.oid)
    }
}

/// The columns of `pg_type` that the wire format and the type rules read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeInfo {
    pub oid: Oid,
    pub name: &'static str,
    /// `typtype`: `b` for a base type, `p` for a pseudo-type, `r` for a range, `m` for a
    /// multirange.
    pub kind: u8,
    /// `typcategory`, the letter that the rules for implicit casts use.
    pub category: u8,
    /// `typlen`: the size in bytes, -1 for a value of variable length and -2 for a C string.
    pub len: i16,
    /// `typelem`: the element type of an array, or 0.
    pub elem: Oid,
    /// `typarray`: the array type of this type, or 0.
    pub array: Oid,
    /// `typdelim`: the character between the elements of an array in the text format.
    pub delim: u8,
    /// `typcollation`: the collation of a type that can have one, or 0 for a type that cannot.
    pub collation: Oid,
}

/// The row constructor of the generated table, short so that each row fits on one line.
#[allow(clippy::too_many_arguments)]
pub(crate) const fn t(
    oid: Oid,
    name: &'static str,
    kind: u8,
    category: u8,
    len: i16,
    elem: Oid,
    array: Oid,
    delim: u8,
    collation: Oid,
) -> TypeInfo {
    TypeInfo { oid, name, kind, category, len, elem, array, delim, collation }
}

impl TypeInfo {
    /// The built-in type with this OID.
    pub fn get(oid: Oid) -> Option<&'static TypeInfo> {
        TYPES.binary_search_by_key(&oid, |info| info.oid).ok().map(|i| &TYPES[i])
    }

    /// The built-in type with this `typname`, such as `int4` or `_int4`.
    pub fn by_name(name: &str) -> Option<&'static TypeInfo> {
        TYPES.iter().find(|info| info.name == name)
    }

    /// Every built-in type, in the order of the OIDs.
    pub fn all() -> &'static [TypeInfo] {
        &TYPES
    }

    /// Whether the type is an array in the sense of SQL, which `format_type` writes as the
    /// element type and `[]`. This is `IsTrueArrayType` with a storage that is not plain. So
    /// `int2vector` and `oidvector` are not arrays, but `_record`, which is a pseudo-type, is.
    pub fn is_array(&self) -> bool {
        match self.oid {
            oid::INT2VECTOR | oid::OIDVECTOR => false,
            oid::RECORD_ARRAY => true,
            _ => self.category == b'A' && self.elem != 0,
        }
    }
}

/// The name of a type as `format_type(oid, NULL)` gives it: the SQL name of a type that has
/// one, such as `integer` for `int4`, the element type and `[]` for an array, `-` for OID 0 and
/// `???` for an OID that is not a type. The row types of the catalogs that have a fixed OID are
/// not in [`TypeInfo::all`], but they have their name here.
pub fn format_type(oid: Oid) -> Cow<'static, str> {
    let name = match oid {
        0 => "-",
        oid::BOOL => "boolean",
        oid::CHAR => "\"char\"",
        oid::INT8 => "bigint",
        oid::INT2 => "smallint",
        oid::INT4 => "integer",
        oid::FLOAT4 => "real",
        oid::FLOAT8 => "double precision",
        oid::BPCHAR => "character",
        oid::VARCHAR => "character varying",
        oid::TIME => "time without time zone",
        oid::TIMESTAMP => "timestamp without time zone",
        oid::TIMESTAMPTZ => "timestamp with time zone",
        oid::TIMETZ => "time with time zone",
        oid::VARBIT => "bit varying",
        oid::ANY => "\"any\"",
        71 => "pg_type",
        75 => "pg_attribute",
        81 => "pg_proc",
        83 => "pg_class",
        1248 => "pg_database",
        2173 => "pg_parameter_acl",
        2842 => "pg_authid",
        2843 => "pg_auth_members",
        4066 => "pg_shseclabel",
        6101 => "pg_subscription",
        _ => match TypeInfo::get(oid) {
            Some(info) if info.is_array() => {
                return Cow::Owned(format!("{}[]", format_type(info.elem)));
            }
            Some(info) => info.name,
            None => "???",
        },
    };
    Cow::Borrowed(name)
}

/// The name of a type as `format_type_with_typemod` gives it, which is the name of
/// [`format_type_extended`] with the modifier given. An interval modifier that has no meaning,
/// for which PostgreSQL fails, gives the name with no modifier.
pub fn format_type_with_typmod(oid: Oid, typmod: i32) -> String {
    format_type_extended(oid, typmod, true).unwrap_or_else(|_| format_type(oid).into_owned())
}

/// `format_type_extended` in `format_type.c` for a built-in type, with `FORMAT_TYPE_ALLOW_INVALID`:
/// OID 0 is `-` and an OID that is no type is `???`. `given` is `FORMAT_TYPE_TYPEMOD_GIVEN`, which
/// `format_type(oid, typmod)` sets when the modifier is not null.
///
/// A modifier of 0 or more is written the way the `typmodout` of the type writes it, or as `(n)`
/// for a type with no `typmodout`. A modifier of -1 that was given is no modifier, with one
/// exception: `bpchar` and `bit` with no length are not `character` and `bit`, which mean a length
/// of 1, so they keep their own names. An array has the modifier of its element.
pub fn format_type_extended(oid: Oid, typmod: i32, given: bool) -> Result<String, TypeError> {
    if let Some(info) = TypeInfo::get(oid).filter(|info| info.is_array()) {
        return Ok(format!("{}[]", format_type_extended(info.elem, typmod, given)?));
    }
    let with = given && typmod >= 0;
    let sql = match oid {
        oid::BOOL | oid::INT2 | oid::INT4 | oid::INT8 | oid::FLOAT4 | oid::FLOAT8 => {
            return Ok(format_type(oid).into_owned());
        }
        oid::BIT => "bit",
        oid::BPCHAR => "character",
        oid::NUMERIC => "numeric",
        oid::INTERVAL => "interval",
        oid::TIME | oid::TIMETZ => "time",
        oid::TIMESTAMP | oid::TIMESTAMPTZ => "timestamp",
        oid::VARBIT => "bit varying",
        oid::VARCHAR => "character varying",
        _ => {
            // The row types of the catalogs are not in the table, and [`format_type`] has their
            // names.
            let name = match TypeInfo::get(oid) {
                Some(info) => quote_identifier(info.name).into_owned(),
                None => format_type(oid).into_owned(),
            };
            if matches!(name.as_str(), "-" | "???") {
                return Ok(name);
            }
            return print_typmod(name, oid, typmod, with);
        }
    };
    match oid {
        _ if with => print_typmod(sql.to_owned(), oid, typmod, true),
        oid::BIT => Ok(if given { "\"bit\"" } else { sql }.to_owned()),
        oid::BPCHAR => Ok(if given { "bpchar" } else { sql }.to_owned()),
        _ => Ok(format_type(oid).into_owned()),
    }
}

/// `printTypmod` in `format_type.c`: the name and the text of the `typmodout` of the type, or the
/// modifier in parentheses for a type that has no `typmodout`.
fn print_typmod(name: String, oid: Oid, typmod: i32, with: bool) -> Result<String, TypeError> {
    if !with {
        return Ok(name);
    }
    let zone = |zoned: bool| if zoned { " with time zone" } else { " without time zone" };
    let out = match oid {
        oid::BPCHAR | oid::VARCHAR => {
            char_length(typmod).map(|length| format!("({length})")).unwrap_or_default()
        }
        oid::NUMERIC => numeric_precision_scale(typmod)
            .map(|(precision, scale)| format!("({precision},{scale})"))
            .unwrap_or_default(),
        oid::BIT | oid::VARBIT => format!("({typmod})"),
        oid::TIME | oid::TIMESTAMP => format!("({typmod}){}", zone(false)),
        oid::TIMETZ | oid::TIMESTAMPTZ => format!("({typmod}){}", zone(true)),
        oid::INTERVAL => interval_typmod_out(typmod)?,
        _ => format!("({typmod})"),
    };
    Ok(name + &out)
}

/// `intervaltypmodout` in `timestamp.c`: the fields of the range and the precision.
fn interval_typmod_out(typmod: i32) -> Result<String, TypeError> {
    use IntervalField::{Day, Hour, Minute, Month, Second, Year};
    let (precision, range) = interval_precision_range(typmod).unwrap_or((0, 0));
    let fields = [
        (&[Year][..], " year"),
        (&[Month], " month"),
        (&[Day], " day"),
        (&[Hour], " hour"),
        (&[Minute], " minute"),
        (&[Second], " second"),
        (&[Year, Month], " year to month"),
        (&[Day, Hour], " day to hour"),
        (&[Day, Hour, Minute], " day to minute"),
        (&[Day, Hour, Minute, Second], " day to second"),
        (&[Hour, Minute], " hour to minute"),
        (&[Hour, Minute, Second], " hour to second"),
        (&[Minute, Second], " minute to second"),
    ];
    let text = match fields.iter().find(|(fields, _)| interval_range(fields) == range) {
        Some((_, text)) => *text,
        None if range == INTERVAL_FULL_RANGE => "",
        None => {
            return Err(TypeError::new(
                SqlState::INTERNAL_ERROR,
                format!("invalid INTERVAL typmod: {typmod:#x}"),
            ));
        }
    };
    Ok(match precision {
        INTERVAL_FULL_PRECISION => text.to_owned(),
        _ => format!("{text}({precision})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oid;

    #[test]
    fn the_table_has_the_types_that_clients_ask_for() {
        // The OIDs that every driver has in a table of its own.
        for (oid, name) in [
            (oid::BOOL, "bool"),
            (oid::BYTEA, "bytea"),
            (oid::CHAR, "char"),
            (oid::NAME, "name"),
            (oid::INT8, "int8"),
            (oid::INT2, "int2"),
            (oid::INT4, "int4"),
            (oid::TEXT, "text"),
            (oid::OID, "oid"),
            (oid::FLOAT4, "float4"),
            (oid::FLOAT8, "float8"),
            (oid::UNKNOWN, "unknown"),
            (oid::VARCHAR, "varchar"),
            (oid::BPCHAR, "bpchar"),
            (oid::DATE, "date"),
            (oid::TIMESTAMPTZ, "timestamptz"),
            (oid::NUMERIC, "numeric"),
            (oid::UUID, "uuid"),
            (oid::JSONB, "jsonb"),
            (oid::INT4_ARRAY, "_int4"),
            (oid::TEXT_ARRAY, "_text"),
            (oid::RECORD, "record"),
        ] {
            assert_eq!(TypeInfo::get(oid).map(|info| info.name), Some(name));
            assert_eq!(TypeInfo::by_name(name).map(|info| info.oid), Some(oid));
        }
        assert_eq!(
            [oid::BOOL, oid::INT4, oid::TEXT, oid::INT4_ARRAY, oid::RECORD],
            [16, 23, 25, 1007, 2249]
        );
    }

    #[test]
    fn an_array_points_at_its_element_and_back() {
        let int4 = PgType::new(oid::INT4).info().unwrap();
        let array = TypeInfo::get(int4.array).unwrap();
        assert!(array.is_array() && !int4.is_array());
        assert_eq!((array.elem, array.len, array.delim), (oid::INT4, -1, b','));
        assert_eq!(TypeInfo::get(oid::BOX_ARRAY).unwrap().delim, b';');
        // `name` is an array of `char` for subscripts, but its category is not `A`.
        assert!(!TypeInfo::get(oid::NAME).unwrap().is_array());
        assert!(TypeInfo::all().windows(2).all(|w| w[0].oid < w[1].oid));
        assert_eq!(TypeInfo::get(0), None);
        let int2vector = TypeInfo::get(oid::INT2VECTOR).unwrap();
        assert!(int2vector.category == b'A' && !int2vector.is_array());
        assert!(TypeInfo::get(oid::RECORD_ARRAY).unwrap().is_array());
    }

    #[test]
    fn format_type_gives_the_sql_name() {
        assert_eq!(format_type(oid::INT4), "integer");
        assert_eq!(format_type(oid::INT4_ARRAY), "integer[]");
        assert_eq!(format_type(oid::INT2VECTOR), "int2vector");
        assert_eq!(format_type(oid::RECORD_ARRAY), "record[]");
        assert_eq!(format_type(oid::TEXT), "text");
        assert_eq!((format_type(0), format_type(9999)), ("-".into(), "???".into()));
    }

    #[test]
    fn a_modifier_is_written_the_way_typmodout_writes_it() {
        for (oid, typmod, name) in [
            (oid::INT4, -1, "integer"),
            (oid::BPCHAR, -1, "bpchar"),
            (oid::BPCHAR, 9, "character(5)"),
            (oid::VARCHAR, 14, "character varying(10)"),
            (oid::NUMERIC, (10 << 16 | 2) + 4, "numeric(10,2)"),
            (oid::NUMERIC, (3 << 16 | 0x7ff) + 4, "numeric(3,-1)"),
            (oid::TIMESTAMPTZ, 3, "timestamp(3) with time zone"),
            (oid::VARCHAR_ARRAY, 14, "character varying(10)[]"),
        ] {
            assert_eq!(format_type_with_typmod(oid, typmod), name);
        }
    }

    /// The answers of `format_type(oid, typmod)` of PostgreSQL 19, with `None` for a null typmod.
    #[test]
    fn format_type_gives_the_names_of_postgres() {
        for (oid, typmod, name) in [
            (oid::BPCHAR, None, "character"),
            (oid::BPCHAR, Some(-1), "bpchar"),
            (oid::BPCHAR, Some(0), "character"),
            (oid::BPCHAR, Some(5), "character(1)"),
            (oid::BIT, None, "bit"),
            (oid::BIT, Some(-1), "\"bit\""),
            (oid::BIT, Some(0), "bit(0)"),
            (oid::CHAR, Some(5), "\"char\"(5)"),
            (oid::ANY, Some(0), "\"any\"(0)"),
            (oid::TEXT, Some(5), "text(5)"),
            (oid::INT4, Some(5), "integer"),
            (oid::INT4_ARRAY, Some(0), "integer[]"),
            (oid::NUMERIC, Some(0), "numeric"),
            (oid::NUMERIC, Some(65543), "numeric(1,3)"),
            (oid::VARCHAR_ARRAY, Some(5), "character varying(1)[]"),
            (oid::DATE, Some(0), "date(0)"),
            (oid::TIME, Some(0), "time(0) without time zone"),
            (oid::TIMESTAMPTZ, None, "timestamp with time zone"),
            (oid::INTERVAL, None, "interval"),
            (oid::INTERVAL, Some(-1), "interval"),
            (oid::INTERVAL, Some(327679), "interval year"),
            (oid::INTERVAL, Some(458751), "interval year to month"),
            (oid::INTERVAL, Some(470286338), "interval day to second(2)"),
            (oid::INTERVAL, Some(2147418115), "interval(3)"),
            (oid::INTERVAL, Some(268435460), "interval second(4)"),
            (0, Some(5), "-"),
            (999999, None, "???"),
        ] {
            let given = typmod.is_some();
            let found = format_type_extended(oid, typmod.unwrap_or(-1), given);
            assert_eq!(found.as_deref(), Ok(name), "{oid} {typmod:?}");
        }
        let error = format_type_extended(oid::INTERVAL, 0, true).unwrap_err();
        assert_eq!(error.message, "invalid INTERVAL typmod: 0x0");
    }
}
