//! The PostgreSQL types: OIDs, typmods, and the text and binary forms of each type.
//!
//! Rank 2 in the layer rule. See `xtask/layers.toml` and `notes/Spec/2140/compat/postgres/16-crate-layout.md`.
//!
//! A client reads the type of a column from the OID and the typmod in `RowDescription`, and it
//! parses each value with the rules of that type. So each input function here takes what the
//! function of PostgreSQL takes and refuses what it refuses, with the same SQLSTATE and the same
//! message, and each output function writes the same bytes. Document 06 of the PostgreSQL
//! compatibility notes is the specification, and the server at the pin is the reference.
//!
//! # What is here
//!
//! [`oid`], one constant for each type in `pg_type.dat` at the pin and for its array type, and
//! [`TypeInfo`], the row of `pg_type` that the protocol needs. [`PgType`] is an OID with a
//! typmod, and [`typmod`] encodes and decodes the typmod of `varchar`, `numeric` and `interval`.
//! The row types of the shared catalogs, such as `pg_database`, come from the catalog headers and
//! not from `pg_type.dat`, so they are not here.
//!
//! [`common_type`] is `select_common_type`: the type of the values of a `CASE`, a `COALESCE` or
//! an `ARRAY`. It reads the preferred types of `pg_type.dat` and the implicit casts of
//! `pg_cast.dat`, which [`can_coerce_implicitly`] also answers from. [`can_coerce_assigned`] adds
//! the assignment casts.
//!
//! The text forms of `bool`, `"char"`, `name`, `int2`, `int4`, `int8`, `oid`, `float4`,
//! `float8`, `bytea` and `uuid`. An input function takes the string and gives the value or a
//! [`TypeError`]. An output function appends to a buffer, so the row encoder can write a whole row
//! into one buffer with no allocation per value. [`float8_out`] and [`float4_out`] take
//! `extra_float_digits`: above zero they give the shortest text that reads back to the same value,
//! and at zero or below they give the `%g` text of old servers.
//!
//! `text`, `varchar(n)`, `character(n)` and `json`. The text form of `text` is the string.
//! [`varchar_in`] and [`bpchar_in`] take the typmod, refuse a value that is too long, and remove
//! the spaces after the length. [`bpchar_in`] also pads a short value with spaces.
//! [`varchar_coerce`] and [`bpchar_coerce`] are the length casts, which cut with no error when the
//! cast is explicit. [`json_in`] checks the syntax with the error details of PostgreSQL and gives
//! the string. [`Recv::text`] is the encoding check of the receive function of each string type.
//!
//! [`Numeric`], a `numeric` value in the layout of PostgreSQL, with [`numeric_in`],
//! [`numeric_out`], [`numeric_recv`] and [`numeric_send`]. The input and the receive function
//! take the typmod and round to it as PostgreSQL does. The engine keeps a `numeric(p, s)` column
//! with p up to 38 as a scaled `i128`, and [`decimal_out`] and [`decimal_send`] write that integer
//! with no allocation.
//!
//! `to_char` of a number and `to_number` with a [`NumberTemplate`]: [`int4_to_char`],
//! [`int8_to_char`], [`numeric_to_char`], [`float4_to_char`], [`float8_to_char`] and
//! [`to_number`], as `formatting.c` writes and reads them in the C locale.
//!
//! `date`, `time`, `timetz`, `timestamp`, `timestamptz` and `interval` in the layout of
//! PostgreSQL: days or microseconds since 2000-01-01, with the infinities at the ends of the
//! integer range. The output functions take the `DateStyle` as a [`DateFormat`] and the
//! `IntervalStyle` as an [`IntervalStyle`], and [`timestamptz_out`] takes a [`TimeZone`] that
//! gives the offset and the abbreviation at an instant. The receive functions check the range and
//! round to the typmod. [`date2j`] and [`j2date`] convert between a calendar date and a Julian day.
//!
//! The text input of the same types, [`date_in`], [`time_in`], [`timetz_in`], [`timestamp_in`],
//! [`timestamptz_in`] and [`interval_in`], is a port of the decoder in `datetime.c`. It reads the
//! session state from a [`DateTimeInput`]: the field order, the time zone, the zone names, the
//! time zone abbreviations and the time of the transaction start. [`ZoneAbbrevs`] reads a file of
//! `src/timezone/tznames`, and [`ZoneAbbrevs::postgres_default`] is the `Default` set.
//!
//! [`Array`], an array of any element type with up to [`MAXDIM`] dimensions and any lower bounds.
//! [`array_in`], [`array_out`], [`array_recv`] and [`array_send`] take the function of the element
//! type as a closure and give the errors of PostgreSQL, also the error of the element. The binary
//! input refuses an element type that is not the expected type, and [`format_type`] gives the
//! names in that error. `int2vector` and `oidvector` are arrays with the lower bound 0 and a text
//! form with spaces, in [`int2vector_in`], [`oidvector_in`] and the functions beside them.
//!
//! [`RegKind`], the OID alias types such as `regclass` and `regtype`. [`reg_in`] reads a number or
//! `-` as PostgreSQL does and gives any other string as a name, and [`reg_out_oid`] writes OID 0
//! and an OID with no object. The catalog finds the names: it splits a name such as
//! `schema.table` with [`qualified_name_list`] and writes the names of the objects.
//!
//! [`RowEncoder`], the `DataRow` encoder. It takes a batch of engine vectors and writes one
//! `DataRow` message for each row in a range, in the text or binary format of each column. It
//! computes the length of each value first, then grows the buffer once and writes each value in
//! place, so a row has no allocation for each value. [`OutputSettings`] holds the session state
//! that the output depends on, such as `DateStyle`, `IntervalStyle`, `extra_float_digits`,
//! `bytea_output` and the time zone. [`date_from_unix`] and [`timestamp_from_unix`] convert the
//! engine values, which count from 1970, to the values of PostgreSQL, which count from 2000.
//!
//! [`Recv`], the binary input of a `Bind` parameter, with the errors of PostgreSQL when the value
//! is too short or too long. The binary output of the other types is the value in big-endian bytes.
//!
//! # Known differences
//!
//! The input functions take a string in the server encoding, which is UTF-8. The caller converts
//! from the client encoding first.
//!
//! [`FixedZone`] and the session zone of the engine, `rudb_common::SessionTimeZone`, implement
//! [`TimeZone`]. [`NoZones`] and [`SessionZones`] implement [`ZoneLookup`]. The session zone gives
//! no meaning of its own to an abbreviation, so an abbreviation is read from
//! `timezone_abbreviations` only.
//!
//! The `TM` prefix of a `to_char` template gives the names of the C locale, which are the English
//! names, because the engine has no `lc_time`.
//!
//! [`RowEncoder`] writes text in UTF-8 and does not convert to the client encoding. It flattens a
//! constant or dictionary vector before it writes the rows, so it does not yet use the shape of
//! the vector to write a repeated value once.
//!
//! PostgreSQL parses `json` by recursion and stops a deep value with `stack depth limit exceeded`
//! when it reaches `max_stack_depth`. [`json_in`] uses no recursion and takes a value at any
//! depth.

mod array;
mod binary;
mod coerce;
mod collations;
mod datetime;
mod declared;
mod error;
mod float;
mod generated;
mod json;
mod jsonb;
pub mod keywords;
mod number;
mod number_format;
mod numeric;
mod opclasses;
mod operators;
mod param;
mod polymorphic;
mod procs;
mod reg;
mod resolve;
mod row;
mod scalar;
mod string;
mod types;
pub mod typmod;

pub use array::{
    Array, ArrayDim, MAX_ARRAY_SIZE, MAXDIM, array_in, array_out, array_recv, array_send,
    check_bounds, int2vector_in, int2vector_out, int2vector_recv, int2vector_send, item_count,
    oidvector_in, oidvector_out, oidvector_recv, oidvector_send,
};
pub use binary::{Recv, name_recv, verify_utf8};
pub use coerce::{
    Cast, CoercionContext, CoercionPath, Mismatch, can_coerce_assigned, can_coerce_implicitly,
    common_type, find_cast, find_coercion_pathway, is_preferred,
};
pub use collations::{Collation, collation, collation_by_oid};
pub use datetime::{
    Abbrev, AbbrevMeaning, DATE_INFINITY, DATE_NEGATIVE_INFINITY, DateFormat, DateOrder, DateStyle,
    DateTemplate, DateTimeInput, FixedZone, Interval, IntervalStyle, NoZones, POSTGRES_EPOCH_JDATE,
    SessionZones, TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, TimeZone, UNIX_EPOCH_JDATE,
    UNIX_TO_POSTGRES_DAYS, UNIX_TO_POSTGRES_USECS, USECS_PER_DAY, USECS_PER_SEC, ZoneAbbrevs,
    ZoneLookup, date_from_unix, date_in, date_out, date_recv, date2j, interval_in, interval_out,
    interval_recv, interval_send, interval_to_char, j2date, time_in, time_out, time_recv,
    timestamp_from_unix, timestamp_in, timestamp_out, timestamp_recv, timestamp_to_char,
    timestamp_to_unix, timestamptz_in, timestamptz_out, timestamptz_to_char, timetz_in, timetz_out,
    timetz_recv, to_date, to_timestamp,
};
pub use declared::{declared_type, session_type, type_name};
pub use error::TypeError;
pub use float::{float4_in, float4_out, float8_in, float8_out};
pub use generated::oids as oid;
pub use json::json_in;
pub use jsonb::{jsonb_in, jsonb_recv, jsonb_send};
pub use number::{int_out, int2_in, int4_in, int8_in, oid_in, oid_out, u64_out};
pub use number_format::{
    NumberTemplate, float4_to_char, float8_to_char, int4_to_char, int8_to_char, numeric_to_char,
    to_number,
};
pub use numeric::{
    Numeric, NumericSign, decimal_out, decimal_send, numeric_in, numeric_out, numeric_out_sci,
    numeric_recv, numeric_send,
};
pub use opclasses::{
    Amop, Opclass, OrderingOperator, binary_coercible, default_opclass, equality_for_ordering,
    has_equality, has_ordering, ordering_operator,
};
pub use operators::{Operator, hashable, operators, operators_of};
pub use param::{
    InputSettings, column_value, has_plain_input, logical_type, param_value, plain_text_value,
    soft_input_error,
};
pub use polymorphic::{Generic, enforce_generic_types};
pub use procs::{Proc, builtin_procs, func_name_as_type, procs};
pub use reg::{
    RegInput, RegKind, qualified_name_list, reg_in, reg_out_oid, split_identifier_string,
};
pub use resolve::{
    Call, Candidate, Failure, OperatorResolution, Resolution, is_polymorphic, resolve_call,
    resolve_function, resolve_operator,
};
pub use row::{OutputSettings, RowEncoder, encodable, pg_type, text_values};
pub use scalar::{
    ByteaOutput, NAME_MAX_BYTES, bool_in, bool_out, bytea_in, bytea_out, char_in, char_out,
    name_in, uuid_in, uuid_out,
};
pub use string::{bpchar_coerce, bpchar_in, varchar_coerce, varchar_in};
pub use types::{
    Oid, PgType, TypeInfo, format_type, format_type_extended, format_type_with_typmod,
};
