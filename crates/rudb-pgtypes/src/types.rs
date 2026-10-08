//! The type of a PostgreSQL value as a client sees it, and the facts of each built-in type.

use std::borrow::Cow;

use crate::generated::oids::TYPES;
use crate::oid;

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

/// The name of a type as `format_type(oid, typmod)` gives it, which is [`format_type`] with the
/// modifier written the way the type's `typmodout` writes it.
///
/// A modifier of -1 is no modifier, with the one exception PostgreSQL makes: `bpchar` with no
/// length is not `character`, which means `character(1)`, so it keeps its own name. An array has
/// the modifier of its element.
pub fn format_type_with_typmod(oid: Oid, typmod: i32) -> String {
    if let Some(info) = TypeInfo::get(oid).filter(|info| info.is_array()) {
        return format!("{}[]", format_type_with_typmod(info.elem, typmod));
    }
    if typmod < 0 {
        return match oid {
            oid::BPCHAR => "bpchar".to_owned(),
            _ => format_type(oid).into_owned(),
        };
    }
    let precision = |rest: &str| format!("({typmod}){rest}");
    match oid {
        oid::BPCHAR => format!("character({})", typmod - 4),
        oid::VARCHAR => format!("character varying({})", typmod - 4),
        oid::NUMERIC => {
            let packed = typmod - 4;
            let scale = ((packed & 0x7ff) ^ 1024) - 1024;
            format!("numeric({},{scale})", (packed >> 16) & 0xffff)
        }
        oid::BIT => format!("bit({typmod})"),
        oid::VARBIT => format!("bit varying({typmod})"),
        oid::TIME => format!("time{}", precision(" without time zone")),
        oid::TIMETZ => format!("time{}", precision(" with time zone")),
        oid::TIMESTAMP => format!("timestamp{}", precision(" without time zone")),
        oid::TIMESTAMPTZ => format!("timestamp{}", precision(" with time zone")),
        _ => format_type(oid).into_owned(),
    }
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
}
