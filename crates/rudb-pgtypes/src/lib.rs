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
//! The text forms of `bool`, `"char"`, `name`, `int2`, `int4`, `int8`, `oid`, `float4`,
//! `float8`, `bytea` and `uuid`. An input function takes the string and gives the value or a
//! [`TypeError`]. An output function appends to a buffer, so the row encoder can write a whole row
//! into one buffer with no allocation per value. [`float8_out`] and [`float4_out`] take
//! `extra_float_digits`: above zero they give the shortest text that reads back to the same value,
//! and at zero or below they give the `%g` text of old servers.
//!
//! [`Numeric`], a `numeric` value in the layout of PostgreSQL, with [`numeric_in`],
//! [`numeric_out`], [`numeric_recv`] and [`numeric_send`]. The input and the receive function
//! take the typmod and round to it as PostgreSQL does. The engine keeps a `numeric(p, s)` column
//! with p up to 38 as a scaled `i128`, and [`decimal_out`] and [`decimal_send`] write that integer
//! with no allocation.
//!
//! [`Recv`], the binary input of a `Bind` parameter, with the errors of PostgreSQL when the value
//! is too short or too long. The binary output of the other types is the value in big-endian bytes.
//!
//! # Known differences
//!
//! The input functions take a string in the server encoding, which is UTF-8. The caller converts
//! from the client encoding first.

mod binary;
mod error;
mod float;
mod generated;
mod number;
mod numeric;
mod scalar;
mod types;
pub mod typmod;

pub use binary::{Recv, name_recv};
pub use error::TypeError;
pub use float::{float4_in, float4_out, float8_in, float8_out};
pub use generated::oids as oid;
pub use number::{int_out, int2_in, int4_in, int8_in, oid_in, oid_out, u64_out};
pub use numeric::{
    Numeric, NumericSign, decimal_out, decimal_send, numeric_in, numeric_out, numeric_recv,
    numeric_send,
};
pub use scalar::{
    ByteaOutput, NAME_MAX_BYTES, bool_in, bool_out, bytea_in, bytea_out, char_in, char_out,
    name_in, uuid_in, uuid_out,
};
pub use types::{Oid, PgType, TypeInfo};
