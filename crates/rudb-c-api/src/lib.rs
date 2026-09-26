//! A libduckdb-compatible C surface, plus a native C surface.
//!
//! Rank 15 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This crate is allowed `unsafe`, per `spec/16-testing.md` section 16.7. Every block carries
//! the invariant that makes it sound, and the lint in the workspace manifest is what turns that
//! into a build failure rather than a habit.
//!
//! What is here is the part of `duckdb.h` a program needs to open a database, run statements, read
//! a result back a value at a time, and load rows through the Appender (`07-the-head.md` section
//! 7.11). The signatures and the struct layouts are the header's, so a program compiled against
//! `duckdb.h` links against this library unchanged for the functions it has.
//!
//! Every handle is a pointer to a Rust value boxed here. The header declares each as a pointer to a
//! struct holding one `internal_ptr`, and no caller looks inside, so the pointer is all that has to
//! agree. A string this library hands out is a `CString` it allocated, and [`duckdb_free`] takes it
//! back, which is the one kind of allocation the functions here hand out.

#![allow(non_camel_case_types, unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

use rudb::{Appender, Connection, Database, QueryResult};
use rudb_common::{ErrorCode, Value};

/// The crate this rank belongs to, so that the layer check has something to read.
pub const RANK: u8 = 15;

/// `duckdb_state`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum duckdb_state {
    /// `DuckDBSuccess`.
    DuckDBSuccess = 0,
    /// `DuckDBError`.
    DuckDBError = 1,
}

use duckdb_state::{DuckDBError, DuckDBSuccess};

/// `idx_t`.
pub type idx_t = u64;

/// What a `duckdb_database` points at.
#[derive(Debug)]
pub struct DatabaseHandle(Database);

/// What a `duckdb_connection` points at.
#[derive(Debug)]
pub struct ConnectionHandle {
    db: Database,
    connection: Connection,
}

/// What a `duckdb_appender` points at.
#[derive(Debug)]
pub struct AppenderHandle {
    appender: Option<Appender>,
    error: Option<CString>,
}

/// `duckdb_database`.
pub type duckdb_database = *mut DatabaseHandle;
/// `duckdb_connection`.
pub type duckdb_connection = *mut ConnectionHandle;
/// `duckdb_appender`.
pub type duckdb_appender = *mut AppenderHandle;

/// What a result's `internal_data` points at.
#[derive(Debug)]
struct ResultData {
    result: Option<QueryResult>,
    error: Option<CString>,
}

/// `duckdb_result`, field for field.
#[repr(C)]
#[derive(Debug)]
pub struct duckdb_result {
    deprecated_column_count: idx_t,
    deprecated_row_count: idx_t,
    deprecated_rows_changed: idx_t,
    deprecated_columns: *mut c_void,
    deprecated_error_message: *mut c_char,
    internal_data: *mut c_void,
}

/// A message as a C string, with any interior nul cut off rather than lost.
fn message(text: impl Into<String>) -> CString {
    let mut text = text.into();
    if let Some(at) = text.find('\0') {
        text.truncate(at);
    }
    CString::new(text).unwrap_or_default()
}

/// A C string argument, or `None` for a null pointer or one that is not UTF-8.
///
/// # Safety
///
/// `text` is null or points at a nul-terminated string that outlives the call.
unsafe fn text<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: not null, and the caller promises a nul-terminated string.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// Opens a database file, or one in memory for a null path or `:memory:`.
///
/// # Safety
///
/// `path` is null or a nul-terminated string, and `out` is a valid pointer to write to.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_open(
    path: *const c_char,
    out: *mut duckdb_database,
) -> duckdb_state {
    if out.is_null() {
        return DuckDBError;
    }
    // SAFETY: the caller's promise about `path`.
    let opened = match unsafe { text(path) } {
        None | Some("" | ":memory:") => Ok(Database::new()),
        Some(path) => Database::open(path),
    };
    let (handle, state) = match opened {
        Ok(db) => (Box::into_raw(Box::new(DatabaseHandle(db))), DuckDBSuccess),
        Err(_) => (ptr::null_mut(), DuckDBError),
    };
    // SAFETY: `out` is not null and the caller promises it is writable.
    unsafe { *out = handle };
    state
}

/// Closes a database and sets the handle to null.
///
/// # Safety
///
/// `db` is null or points at a handle [`duckdb_open`] wrote, which is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_close(db: *mut duckdb_database) {
    if db.is_null() {
        return;
    }
    // SAFETY: not null, and the caller promises it holds a handle from `duckdb_open` or null.
    let handle = unsafe { std::mem::replace(&mut *db, ptr::null_mut()) };
    if !handle.is_null() {
        // SAFETY: the handle came from `Box::into_raw` and is dropped once, here.
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// Opens a connection on a database.
///
/// # Safety
///
/// `db` is a handle from [`duckdb_open`] and `out` is a valid pointer to write to.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_connect(
    db: duckdb_database,
    out: *mut duckdb_connection,
) -> duckdb_state {
    if db.is_null() || out.is_null() {
        return DuckDBError;
    }
    // SAFETY: a live handle from `duckdb_open`, per the caller.
    let db = unsafe { &(*db).0 };
    let handle = ConnectionHandle { db: db.clone(), connection: db.connect() };
    // SAFETY: `out` is not null and the caller promises it is writable.
    unsafe { *out = Box::into_raw(Box::new(handle)) };
    DuckDBSuccess
}

/// Closes a connection and sets the handle to null.
///
/// # Safety
///
/// `connection` is null or points at a handle [`duckdb_connect`] wrote, which is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_disconnect(connection: *mut duckdb_connection) {
    if connection.is_null() {
        return;
    }
    // SAFETY: not null, and the caller promises it holds a handle from `duckdb_connect` or null.
    let handle = unsafe { std::mem::replace(&mut *connection, ptr::null_mut()) };
    if !handle.is_null() {
        // SAFETY: the handle came from `Box::into_raw` and is dropped once, here.
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// Runs one statement or a script, filling `out` when it is not null.
///
/// # Safety
///
/// `connection` is a handle from [`duckdb_connect`], `sql` is a nul-terminated string, and `out` is
/// null or a valid pointer to write a result to, which is then destroyed with
/// [`duckdb_destroy_result`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_query(
    connection: duckdb_connection,
    sql: *const c_char,
    out: *mut duckdb_result,
) -> duckdb_state {
    // SAFETY: the caller's promise about `sql`.
    let sql = unsafe { text(sql) };
    let answer = match (connection.is_null(), sql) {
        (false, Some(sql)) => {
            // SAFETY: a live handle from `duckdb_connect`, per the caller.
            unsafe { &(*connection).connection }.execute(sql).map_err(|error| error.to_string())
        }
        _ => Err("Invalid Input Error: no connection or no query".to_string()),
    };
    let state = if answer.is_ok() { DuckDBSuccess } else { DuckDBError };
    if out.is_null() {
        return state;
    }
    let data = match answer {
        Ok(result) => ResultData { result: Some(result), error: None },
        Err(error) => ResultData { result: None, error: Some(message(error)) },
    };
    let (columns, rows, changed) = data
        .result
        .as_ref()
        .map_or((0, 0, 0), |result| (result.width(), result.len(), result.changes().unwrap_or(0)));
    let mut data = Box::new(data);
    let error = data.error.as_mut().map_or(ptr::null_mut(), |error| error.as_ptr().cast_mut());
    let filled = duckdb_result {
        deprecated_column_count: columns as idx_t,
        deprecated_row_count: rows as idx_t,
        deprecated_rows_changed: changed as idx_t,
        deprecated_columns: ptr::null_mut(),
        deprecated_error_message: error,
        internal_data: Box::into_raw(data).cast(),
    };
    // SAFETY: `out` is not null and the caller promises it is writable.
    unsafe { out.write(filled) };
    state
}

/// The data behind a result, if it has any.
///
/// # Safety
///
/// `result` is null or a result [`duckdb_query`] filled and not yet destroyed.
unsafe fn data<'a>(result: *const duckdb_result) -> Option<&'a ResultData> {
    if result.is_null() {
        return None;
    }
    // SAFETY: not null, and a live result per the caller.
    let internal = unsafe { (*result).internal_data };
    // SAFETY: `internal_data` is null or the `ResultData` `duckdb_query` boxed.
    (!internal.is_null()).then(|| unsafe { &*internal.cast::<ResultData>() })
}

/// Frees what a result holds.
///
/// # Safety
///
/// `result` is null or a result [`duckdb_query`] filled, and is not read again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_destroy_result(result: *mut duckdb_result) {
    if result.is_null() {
        return;
    }
    // SAFETY: not null, and a live result per the caller.
    let result = unsafe { &mut *result };
    if !result.internal_data.is_null() {
        // SAFETY: the pointer came from `Box::into_raw` in `duckdb_query` and is dropped once.
        drop(unsafe { Box::from_raw(result.internal_data.cast::<ResultData>()) });
    }
    result.internal_data = ptr::null_mut();
    result.deprecated_error_message = ptr::null_mut();
}

/// The error of a failed statement, or null. Freed with the result.
///
/// # Safety
///
/// As for [`duckdb_destroy_result`], before it is called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_result_error(result: *mut duckdb_result) -> *const c_char {
    // SAFETY: the caller's promise about `result`.
    unsafe { data(result) }
        .and_then(|data| data.error.as_ref())
        .map_or(ptr::null(), |error| error.as_ptr())
}

/// How many columns a result has.
///
/// # Safety
///
/// As for [`duckdb_result_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_column_count(result: *mut duckdb_result) -> idx_t {
    // SAFETY: the caller's promise about `result`.
    unsafe { data(result) }.and_then(|data| data.result.as_ref()).map_or(0, |r| r.width() as idx_t)
}

/// How many rows a result has.
///
/// # Safety
///
/// As for [`duckdb_result_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_row_count(result: *mut duckdb_result) -> idx_t {
    // SAFETY: the caller's promise about `result`.
    unsafe { data(result) }.and_then(|data| data.result.as_ref()).map_or(0, |r| r.len() as idx_t)
}

/// How many rows an `INSERT`, `UPDATE` or `DELETE` changed, and zero for anything else.
///
/// # Safety
///
/// As for [`duckdb_result_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_rows_changed(result: *mut duckdb_result) -> idx_t {
    // SAFETY: the caller's promise about `result`.
    unsafe { data(result) }
        .and_then(|data| data.result.as_ref())
        .and_then(QueryResult::changes)
        .map_or(0, |changed| changed as idx_t)
}

/// One value of a result, or `None` past the end.
///
/// # Safety
///
/// As for [`duckdb_result_error`].
unsafe fn cell(result: *mut duckdb_result, column: idx_t, row: idx_t) -> Option<(Value, String)> {
    // SAFETY: the caller's promise about `result`.
    let result = unsafe { data(result) }?.result.as_ref()?;
    let (column, row) = (usize::try_from(column).ok()?, usize::try_from(row).ok()?);
    if column >= result.width() || row >= result.len() {
        return None;
    }
    let value = result.value_at(row, column);
    let text = result.text_at(row, column);
    Some((value, text))
}

/// Whether a value is null.
///
/// # Safety
///
/// As for [`duckdb_result_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_is_null(
    result: *mut duckdb_result,
    column: idx_t,
    row: idx_t,
) -> bool {
    // SAFETY: the caller's promise about `result`.
    unsafe { cell(result, column, row) }.is_none_or(|(value, _)| value.is_null())
}

/// A value as a BIGINT, or zero when it is null or does not convert.
///
/// # Safety
///
/// As for [`duckdb_result_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_int64(
    result: *mut duckdb_result,
    column: idx_t,
    row: idx_t,
) -> i64 {
    // SAFETY: the caller's promise about `result`.
    let Some((value, text)) = (unsafe { cell(result, column, row) }) else { return 0 };
    match value {
        Value::Null => 0,
        Value::Boolean(value) => i64::from(value),
        _ => text.trim().parse().unwrap_or(0),
    }
}

/// A value as text, in a string the caller frees with [`duckdb_free`], or null for a null.
///
/// # Safety
///
/// As for [`duckdb_result_error`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_value_varchar(
    result: *mut duckdb_result,
    column: idx_t,
    row: idx_t,
) -> *mut c_char {
    // SAFETY: the caller's promise about `result`.
    match unsafe { cell(result, column, row) } {
        Some((value, text)) if !value.is_null() => message(text).into_raw(),
        _ => ptr::null_mut(),
    }
}

/// Frees a string this library handed out.
///
/// # Safety
///
/// `pointer` is null or a string from [`duckdb_value_varchar`], freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_free(pointer: *mut c_void) {
    if !pointer.is_null() {
        // SAFETY: every pointer this library hands out to be freed is a `CString::into_raw`.
        drop(unsafe { CString::from_raw(pointer.cast()) });
    }
}

/// Creates an Appender on `schema.table`, or `table` in the default schema for a null schema.
///
/// # Safety
///
/// `connection` is a handle from [`duckdb_connect`], `schema` is null or a nul-terminated string,
/// `table` is a nul-terminated string, and `out` is a valid pointer to write to. The Appender is
/// destroyed with [`duckdb_appender_destroy`] even when this fails, the way the header asks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_create(
    connection: duckdb_connection,
    schema: *const c_char,
    table: *const c_char,
    out: *mut duckdb_appender,
) -> duckdb_state {
    if connection.is_null() || out.is_null() {
        return DuckDBError;
    }
    // SAFETY: a live handle from `duckdb_connect`, per the caller.
    let db = unsafe { &(*connection).db };
    // SAFETY: the caller's promise about `schema` and `table`.
    let (schema, table) = unsafe { (text(schema), text(table)) };
    let created = match table {
        None => Err("no table name".to_string()),
        Some(table) => {
            let schema = schema.unwrap_or("main");
            db.appender(&format!("{schema}.{table}")).map_err(|error| match error.code() {
                ErrorCode::Catalog => format!("Table \".{schema}.{table}\" could not be found"),
                _ => error.message().to_owned(),
            })
        }
    };
    let (handle, state) = match created {
        Ok(appender) => (AppenderHandle { appender: Some(appender), error: None }, DuckDBSuccess),
        Err(error) => (AppenderHandle { appender: None, error: Some(message(error)) }, DuckDBError),
    };
    // SAFETY: `out` is not null and the caller promises it is writable.
    unsafe { *out = Box::into_raw(Box::new(handle)) };
    state
}

/// Runs one step of an Appender and keeps its error for [`duckdb_appender_error`].
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
unsafe fn step(
    appender: duckdb_appender,
    run: impl FnOnce(&mut Appender) -> rudb_common::Result<()>,
) -> duckdb_state {
    if appender.is_null() {
        return DuckDBError;
    }
    // SAFETY: not null, and a live handle per the caller.
    let handle = unsafe { &mut *appender };
    let Some(inner) = handle.appender.as_mut() else {
        if handle.error.is_none() {
            handle.error = Some(message("the appender was closed"));
        }
        return DuckDBError;
    };
    match run(inner) {
        Ok(()) => DuckDBSuccess,
        Err(error) => {
            handle.error = Some(message(error.message()));
            DuckDBError
        }
    }
}

/// The last error of an Appender, or null. Freed with the Appender.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_error(appender: duckdb_appender) -> *const c_char {
    if appender.is_null() {
        return ptr::null();
    }
    // SAFETY: not null, and a live handle per the caller.
    unsafe { &*appender }.error.as_ref().map_or(ptr::null(), |error| error.as_ptr())
}

/// How many columns a row of the Appender has.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_column_count(appender: duckdb_appender) -> idx_t {
    if appender.is_null() {
        return 0;
    }
    // SAFETY: not null, and a live handle per the caller.
    unsafe { &*appender }.appender.as_ref().map_or(0, |inner| inner.columns().len() as idx_t)
}

/// Writes every finished row.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_flush(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: the caller's promise about `appender`.
    unsafe { step(appender, Appender::flush) }
}

/// Flushes and closes the Appender. Nothing more can be appended after it.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_close(appender: duckdb_appender) -> duckdb_state {
    if appender.is_null() {
        return DuckDBError;
    }
    // SAFETY: not null, and a live handle per the caller.
    let handle = unsafe { &mut *appender };
    let Some(inner) = handle.appender.take() else { return DuckDBSuccess };
    match inner.close() {
        Ok(()) => DuckDBSuccess,
        Err(error) => {
            handle.error = Some(message(error.message()));
            DuckDBError
        }
    }
}

/// Closes the Appender, frees it, and sets the handle to null.
///
/// # Safety
///
/// `appender` is null or points at a handle [`duckdb_appender_create`] wrote, not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_destroy(appender: *mut duckdb_appender) -> duckdb_state {
    if appender.is_null() {
        return DuckDBError;
    }
    // SAFETY: not null, and the caller promises it holds a handle or null.
    let handle = unsafe { std::mem::replace(&mut *appender, ptr::null_mut()) };
    if handle.is_null() {
        return DuckDBError;
    }
    // SAFETY: the handle is live until it is dropped just below.
    let state = unsafe { duckdb_appender_close(handle) };
    // SAFETY: the handle came from `Box::into_raw` and is dropped once, here.
    drop(unsafe { Box::from_raw(handle) });
    state
}

/// A no-op the header keeps for older callers.
///
/// # Safety
///
/// None needed. It is `unsafe` to match the rest of the surface.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_begin_row(_appender: duckdb_appender) -> duckdb_state {
    DuckDBSuccess
}

/// Finishes the row being appended.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_appender_end_row(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: the caller's promise about `appender`.
    unsafe { step(appender, Appender::end_row) }
}

/// Appends the column's default.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_default(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: the caller's promise about `appender`.
    unsafe { step(appender, Appender::append_default) }
}

/// Appends a null.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_null(appender: duckdb_appender) -> duckdb_state {
    // SAFETY: the caller's promise about `appender`.
    unsafe { step(appender, |inner| inner.append(Value::Null)) }
}

/// One `duckdb_append_<type>` for a type that is a plain number or a bool.
macro_rules! append {
    ($($name:ident($ty:ty) => $variant:ident;)*) => {$(
        /// Appends one value, cast to its column's type.
        ///
        /// # Safety
        ///
        /// `appender` is null or a live handle from [`duckdb_appender_create`].
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(appender: duckdb_appender, value: $ty) -> duckdb_state {
            // SAFETY: the caller's promise about `appender`.
            unsafe { step(appender, |inner| inner.append(Value::$variant(value))) }
        }
    )*};
}

append! {
    duckdb_append_bool(bool) => Boolean;
    duckdb_append_int8(i8) => TinyInt;
    duckdb_append_int16(i16) => SmallInt;
    duckdb_append_int32(i32) => Integer;
    duckdb_append_int64(i64) => BigInt;
    duckdb_append_uint8(u8) => UTinyInt;
    duckdb_append_uint16(u16) => USmallInt;
    duckdb_append_uint32(u32) => UInteger;
    duckdb_append_uint64(u64) => UBigInt;
    duckdb_append_float(f32) => Float;
    duckdb_append_double(f64) => Double;
}

/// Appends a string given as `length` bytes, cast to its column's type.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`], and `value` points at
/// `length` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_varchar_length(
    appender: duckdb_appender,
    value: *const c_char,
    length: idx_t,
) -> duckdb_state {
    let bytes = if value.is_null() || length == 0 {
        &[][..]
    } else {
        let Ok(length) = usize::try_from(length) else { return DuckDBError };
        // SAFETY: the caller promises `length` readable bytes at `value`.
        unsafe { std::slice::from_raw_parts(value.cast::<u8>(), length) }
    };
    let text = String::from_utf8(bytes.to_vec());
    // SAFETY: the caller's promise about `appender`.
    unsafe {
        step(appender, |inner| match text {
            Ok(text) => inner.append(Value::Varchar(text)),
            Err(_) => Err(rudb_common::Error::invalid_input("Invalid unicode detected in string")),
        })
    }
}

/// Appends a nul-terminated string, cast to its column's type.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`], and `value` is a
/// nul-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_varchar(
    appender: duckdb_appender,
    value: *const c_char,
) -> duckdb_state {
    if value.is_null() {
        // SAFETY: the caller's promise about `appender`.
        return unsafe { duckdb_append_null(appender) };
    }
    // SAFETY: the caller promises a nul-terminated string.
    let length = unsafe { CStr::from_ptr(value) }.to_bytes().len();
    // SAFETY: the bytes before the nul are readable, and the promise about `appender` carries.
    unsafe { duckdb_append_varchar_length(appender, value, length as idx_t) }
}

/// Appends `length` bytes as a BLOB.
///
/// # Safety
///
/// `appender` is null or a live handle from [`duckdb_appender_create`], and `data` points at
/// `length` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn duckdb_append_blob(
    appender: duckdb_appender,
    data: *const c_void,
    length: idx_t,
) -> duckdb_state {
    let bytes = if data.is_null() || length == 0 {
        Vec::new()
    } else {
        let Ok(length) = usize::try_from(length) else { return DuckDBError };
        // SAFETY: the caller promises `length` readable bytes at `data`.
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) }.to_vec()
    };
    // SAFETY: the caller's promise about `appender`.
    unsafe { step(appender, |inner| inner.append(Value::Blob(bytes))) }
}
