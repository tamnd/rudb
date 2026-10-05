//! The extended query flow: `Parse`, `Bind`, `Describe`, `Execute` and `Close`.
//!
//! `Parse` prepares the statement in rudb, which parses it but does not bind it, so an error of a
//! table or a column comes at `Describe` or at `Execute` and not at `Parse` as in PostgreSQL. The
//! client sees it before the `Sync` in both cases.
//!
//! `Describe` of a statement binds it without running it, to get the types of the parameters and
//! the columns. `Describe` of a portal runs the portal and keeps the result for the next
//! `Execute`, so a `Bind`, `Describe`, `Execute` sequence runs the statement one time. An
//! `Execute` with a row limit sends part of the result and `PortalSuspended`, and the next
//! `Execute` continues from there.
//!
//! A parameter in the text format of a declared type is read with the input function of that
//! type. A parameter in the text format with no declared type is a `VARCHAR`, which the binder
//! casts to the type it meets. A parameter in the binary format with no declared type is read as
//! the type that `Describe` finds for it.

use std::io;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rudb::{Description, Prepared, QueryResult, Transaction};
use rudb_common::{Fields, LogicalType, Value};
use rudb_pgtypes::{
    DateOrder, DateTimeInput, FixedZone, InputSettings, IntervalStyle, NoZones, Oid, RowEncoder,
    TypeError, TypeInfo, UNIX_TO_POSTGRES_USECS, ZoneAbbrevs, logical_type, param_value, pg_type,
};
use rudb_pgwire::{
    Bind, CommandTag, Field, Level, OutBuf, Portals, ProtocolError, Statements, Target,
};

use super::{FLUSH_AT, Failure, Runner, command_tag, leading_words};

/// The type that PostgreSQL reports for a parameter of no known type.
const TEXT: Oid = 25;

/// An error of a message of the extended flow. It is in a box, because the path without an
/// error must stay small.
pub(super) struct Problem(Box<Kind>);

// `Kind` is only in the box of `Problem`, so the size of its variants does not matter.
#[allow(clippy::large_enum_variant)]
enum Kind {
    Protocol(ProtocolError),
    /// An error with the statement it came from, for the position.
    Failure(Failure, Arc<str>),
}

impl From<ProtocolError> for Problem {
    fn from(error: ProtocolError) -> Problem {
        Problem(Box::new(Kind::Protocol(error)))
    }
}

impl Problem {
    fn failure(failure: Failure, sql: &Arc<str>) -> Problem {
        Problem(Box::new(Kind::Failure(failure, sql.clone())))
    }

    pub(super) fn write(&self, out: &mut OutBuf, protocol: u32) {
        match &*self.0 {
            Kind::Protocol(error) => out.protocol_error(error, protocol),
            Kind::Failure(failure, sql) => failure.write(sql, out),
        }
    }
}

fn aborted() -> Problem {
    let message = "current transaction is aborted, commands ignored until end of transaction block";
    error("25P02", message.to_owned())
}

fn error(sqlstate: &'static str, message: String) -> Problem {
    ProtocolError { level: Level::Error, sqlstate, message, detail: None, hint: None }.into()
}

fn type_failure(error: TypeError, context: Option<String>) -> Failure {
    let fields = context.map(|context| {
        let mut fields = Fields::default();
        fields.context = Some(context);
        fields
    });
    Failure {
        sqlstate: error.sqlstate.as_str().to_owned(),
        message: error.message,
        fields,
        position: None,
    }
}

/// A statement of `Parse`.
pub(super) struct Statement {
    sql: Arc<str>,
    /// `None` for an empty query.
    prepared: Option<Prepared>,
    /// The declared type of each parameter, with 0 for no type.
    types: Vec<Oid>,
    /// The parameter of each name of [`Prepared::parameters`], from 0.
    slots: Vec<usize>,
    /// The names are `1` to `n` for `n` parameters, so the values go by position.
    positional: bool,
}

impl Statement {
    /// The type of each parameter, with the type that the binder found for a parameter of no
    /// declared type, and `text` for one that it did not find.
    fn parameter_types(&self, description: Option<&Description>) -> Vec<Oid> {
        let mut types = self.types.clone();
        if let Some(description) = description {
            for (slot, found) in self.slots.iter().zip(&description.parameters) {
                if types[*slot] == 0
                    && let Some(found) = found
                {
                    types[*slot] = pg_type(found).oid;
                }
            }
        }
        for oid in &mut types {
            if *oid == 0 {
                *oid = TEXT;
            }
        }
        types
    }

    fn describe(&self) -> Result<Option<Description>, Problem> {
        let Some(prepared) = &self.prepared else {
            return Ok(None);
        };
        let declared: Vec<Option<LogicalType>> =
            self.slots.iter().map(|slot| logical_type(self.types[*slot])).collect();
        prepared
            .describe(&declared)
            .map(Some)
            .map_err(|e| Problem::failure(Failure::engine(&e, 0), &self.sql))
    }

    fn execute(&self, prepared: &Prepared, values: &[Value]) -> rudb::Result<QueryResult> {
        if self.positional {
            return prepared.execute(values);
        }
        let named: Vec<(&str, Value)> = prepared
            .parameters()
            .iter()
            .zip(&self.slots)
            .map(|(name, slot)| (name.as_str(), values[*slot].clone()))
            .collect();
        prepared.execute_named(&named)
    }
}

/// A portal of `Bind`.
pub(super) struct Portal {
    statement: Arc<Statement>,
    values: Vec<Value>,
    formats: Vec<i16>,
    ran: Option<Ran>,
}

/// A portal that ran, with the place in its result.
struct Ran {
    result: QueryResult,
    tag: CommandTag,
    /// The statement gives rows, so `Execute` sends them.
    rows: bool,
    /// The `CommandComplete` of a statement without rows went out.
    reported: bool,
    chunk: usize,
    row: usize,
    encoder: Option<RowEncoder>,
}

/// The statements and the portals of a session.
#[derive(Default)]
pub(super) struct Extended {
    statements: Statements<Arc<Statement>>,
    portals: Portals<Portal>,
}

impl Extended {
    /// A `Query` message drops the unnamed statement, as `exec_simple_query` does.
    pub(super) fn simple_query(&mut self) {
        self.statements.close(b"");
    }

    /// The end of a transaction removes the portals.
    pub(super) fn end_of_transaction(&mut self, transaction: Transaction) {
        if transaction == Transaction::Idle && !self.portals.is_empty() {
            self.portals.clear();
        }
    }

    pub(super) fn parse(
        &mut self,
        runner: &Runner<'_>,
        name: &[u8],
        sql: &[u8],
        types: impl Iterator<Item = Oid>,
        out: &mut OutBuf,
    ) -> Result<(), Problem> {
        self.statements.start_parse(name);
        let sql: Arc<str> = std::str::from_utf8(sql)
            .map_err(|_| error("22021", "invalid byte sequence for encoding \"UTF8\"".to_owned()))?
            .into();
        let engine = |e: rudb::Error| Problem::failure(Failure::engine(&e, 0), &sql);
        let prepared = match rudb::statements(&sql).map_err(engine)?.len() {
            0 => None,
            1 => Some(runner.connection.prepare(&sql).map_err(engine)?),
            _ => {
                return Err(error(
                    "42601",
                    "cannot insert multiple commands into a prepared statement".to_owned(),
                ));
            }
        };
        if prepared.is_some()
            && runner.connection.transaction() == Transaction::Aborted
            && !exits_transaction(&sql)
        {
            return Err(aborted());
        }
        let mut types: Vec<Oid> = types.collect();
        let names = prepared.as_ref().map_or(&[][..], Prepared::parameters);
        let numbers: Option<Vec<usize>> = names
            .iter()
            .map(|name| name.parse::<usize>().ok().filter(|n| *n > 0).map(|n| n - 1))
            .collect();
        let slots = numbers.unwrap_or_else(|| (0..names.len()).collect());
        let count = slots.iter().map(|slot| slot + 1).max().unwrap_or(0).max(types.len());
        types.resize(count, 0);
        let positional = count == slots.len()
            && slots.iter().enumerate().all(|(i, s)| {
                // The slots are distinct and below `count`, so they are a permutation of it.
                *s < count && !slots[..i].contains(s)
            });
        let statement = Statement { sql, prepared, types, slots, positional };
        self.statements.insert(name, Arc::new(statement))?;
        out.parse_complete();
        Ok(())
    }

    pub(super) fn bind(
        &mut self,
        runner: &Runner<'_>,
        bind: &Bind<'_>,
        out: &mut OutBuf,
    ) -> Result<(), Problem> {
        let statement = self.statements.get(bind.statement)?.clone();
        let mut params = bind.params(statement.types.len())?;
        if runner.connection.transaction() == Transaction::Aborted
            && (!statement.types.is_empty() || !exits_transaction(&statement.sql))
        {
            return Err(aborted());
        }
        self.portals.make_room(bind.portal)?;
        let zone = FixedZone::utc();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(0));
        let settings = InputSettings {
            datetime: DateTimeInput {
                order: DateOrder::Mdy,
                zone: &zone,
                zones: &NoZones,
                abbrevs: ZoneAbbrevs::postgres_default(),
                now: now + UNIX_TO_POSTGRES_USECS,
            },
            interval_style: IntervalStyle::Postgres,
        };
        let formats = params.formats;
        let mut found: Option<Vec<Oid>> = None;
        let mut values = Vec::with_capacity(statement.types.len());
        for i in 0..statement.types.len() {
            let Some(data) = params.next().transpose()? else {
                break;
            };
            let Some(data) = data else {
                values.push(Value::Null);
                continue;
            };
            let context = || {
                if bind.portal.is_empty() {
                    format!("unnamed portal parameter ${}", i + 1)
                } else {
                    let name = String::from_utf8_lossy(bind.portal);
                    format!("portal \"{name}\" parameter ${}", i + 1)
                }
            };
            let fail =
                |e: TypeError| Problem::failure(type_failure(e, Some(context())), &statement.sql);
            let value = match formats.of(i) {
                0 => {
                    param_value(statement.types[i], false, data, i + 1, &settings).map_err(fail)?
                }
                1 => {
                    let mut oid = statement.types[i];
                    if oid == 0 {
                        if found.is_none() {
                            let description = statement.describe()?;
                            found = Some(statement.parameter_types(description.as_ref()));
                        }
                        oid = found.as_ref().map_or(TEXT, |types| types[i]);
                    }
                    param_value(oid, true, data, i + 1, &settings).map_err(fail)?
                }
                format => {
                    return Err(error("22023", format!("unsupported format code: {format}")));
                }
            };
            values.push(value);
        }
        let formats: Vec<i16> = params.finish()?.iter().collect();
        if formats.len() > 1
            && let Some(fields) = statement.describe()?.and_then(|d| d.fields)
        {
            check_formats(&formats, fields.len())?;
        }
        self.portals.insert(bind.portal, Portal { statement, values, formats, ran: None });
        out.bind_complete();
        Ok(())
    }

    pub(super) fn describe(
        &mut self,
        runner: &Runner<'_>,
        target: Target,
        name: &[u8],
        out: &mut OutBuf,
    ) -> Result<(), Problem> {
        match target {
            Target::Statement => {
                let statement = self.statements.get(name)?;
                let description = statement.describe()?;
                out.parameter_description(&statement.parameter_types(description.as_ref()));
                match description.and_then(|d| d.fields) {
                    Some(fields) => {
                        let columns: Vec<_> =
                            fields.iter().map(|f| (f.name.as_str(), &f.ty, 0)).collect();
                        row_description(&columns, out);
                    }
                    None => out.no_data(),
                }
            }
            Target::Portal => {
                let portal = self.portals.get_mut(name)?;
                let formats = portal.formats.clone();
                match portal.run(runner)? {
                    Some(ran) if ran.rows => {
                        let format = |i: usize| match formats.len() {
                            0 => 0,
                            1 => formats[0],
                            _ => formats[i],
                        };
                        let columns: Vec<_> = ran
                            .result
                            .names()
                            .iter()
                            .zip(ran.result.types())
                            .enumerate()
                            .map(|(i, (name, ty))| (name.as_str(), ty, format(i)))
                            .collect();
                        row_description(&columns, out);
                    }
                    _ => out.no_data(),
                }
            }
        }
        Ok(())
    }

    pub(super) fn execute(
        &mut self,
        runner: &Runner<'_>,
        name: &[u8],
        max_rows: i32,
        out: &mut OutBuf,
        flush: &mut impl FnMut(&mut OutBuf) -> io::Result<()>,
    ) -> io::Result<Result<(), Problem>> {
        let portal = match self.portals.get_mut(name) {
            Ok(portal) => portal,
            Err(e) => return Ok(Err(e.into())),
        };
        let sql = portal.statement.sql.clone();
        let formats = portal.formats.clone();
        let ran = match portal.run(runner) {
            Ok(Some(ran)) => ran,
            Ok(None) => {
                out.empty_query_response();
                return Ok(Ok(()));
            }
            Err(problem) => return Ok(Err(problem)),
        };
        if !ran.rows {
            if ran.reported {
                let name = String::from_utf8_lossy(name);
                return Ok(Err(error("55000", format!("portal \"{name}\" cannot be run"))));
            }
            ran.reported = true;
            out.command_tag(ran.tag, ran.result.changes().unwrap_or(0) as u64);
            return Ok(Ok(()));
        }
        if ran.encoder.is_none() {
            let mut columns = Vec::with_capacity(ran.result.width());
            for (i, logical) in ran.result.types().iter().enumerate() {
                let format = match formats.len() {
                    0 => 0,
                    1 => formats[0],
                    _ => formats[i],
                };
                if format != 0 && format != 1 {
                    let message = format!("unsupported format code: {format}");
                    return Ok(Err(error("22023", message)));
                }
                columns.push((logical.clone(), pg_type(logical).oid, format == 1));
            }
            match RowEncoder::new(&columns) {
                Ok(encoder) => ran.encoder = Some(encoder),
                Err(e) => return Ok(Err(Problem::failure(type_failure(e, None), &sql))),
            }
        }
        let Some(encoder) = ran.encoder.as_mut() else {
            return Ok(Ok(()));
        };
        let limit = if max_rows > 0 { max_rows as u64 } else { u64::MAX };
        let chunks = ran.result.chunks();
        let mut sent = 0u64;
        while sent < limit && ran.chunk < chunks.len() {
            let chunk = match chunks[ran.chunk].clone().settled() {
                Ok(chunk) => chunk,
                Err(e) => return Ok(Err(Problem::failure(Failure::engine(&e, 0), &sql))),
            };
            let n = chunk.len();
            let take = (n - ran.row).min(usize::try_from(limit - sent).unwrap_or(usize::MAX));
            let rows = ran.row..ran.row + take;
            if let Err(e) = encoder.encode(chunk.columns(), rows, &runner.settings, out.bytes_mut())
            {
                return Ok(Err(Problem::failure(type_failure(e, None), &sql)));
            }
            sent += take as u64;
            ran.row += take;
            if ran.row == n {
                ran.chunk += 1;
                ran.row = 0;
            }
            if out.len() >= FLUSH_AT {
                flush(out)?;
            }
        }
        // PostgreSQL stops when it has the rows that the client asked for and does not look for
        // one more, so a limit that is the number of the rows left also suspends the portal.
        if max_rows > 0 && sent == limit {
            out.portal_suspended();
        } else {
            out.command_tag(ran.tag, sent);
        }
        Ok(Ok(()))
    }

    pub(super) fn close(&mut self, target: Target, name: &[u8], out: &mut OutBuf) {
        match target {
            Target::Statement => drop(self.statements.close(name)),
            Target::Portal => drop(self.portals.close(name)),
        }
        out.close_complete();
    }
}

impl Portal {
    /// Runs the portal if it did not run yet. `None` is an empty query.
    fn run(&mut self, runner: &Runner<'_>) -> Result<Option<&mut Ran>, Problem> {
        let Some(prepared) = &self.statement.prepared else {
            return Ok(None);
        };
        if self.ran.is_none() {
            let sql = &self.statement.sql;
            let before = runner.connection.transaction();
            let result = self
                .statement
                .execute(prepared, &self.values)
                .map_err(|e| Problem::failure(Failure::engine(&e, 0), sql))?;
            let tag = command_tag(sql, &result, before);
            let rows =
                result.changes().is_none() && (result.width() > 0 || tag == CommandTag::Select);
            if rows {
                check_formats(&self.formats, result.width())?;
            }
            self.ran =
                Some(Ran { result, tag, rows, reported: false, chunk: 0, row: 0, encoder: None });
        }
        Ok(self.ran.as_mut())
    }
}

/// The check of `PortalSetResultFormat`: one format for each column, or one or none for all.
fn check_formats(formats: &[i16], columns: usize) -> Result<(), Problem> {
    if formats.len() > 1 && formats.len() != columns {
        let count = formats.len();
        let message =
            format!("bind message has {count} result formats but query has {columns} columns");
        return Err(error("08P01", message));
    }
    Ok(())
}

/// True for the statements that `IsTransactionExitStmt` takes: they can run in a failed
/// transaction.
fn exits_transaction(sql: &str) -> bool {
    let words = leading_words(sql);
    matches!(words.first().map(String::as_str), Some("COMMIT" | "END" | "ROLLBACK" | "ABORT"))
}

/// Writes a `RowDescription` for columns of a name, a type and a format.
fn row_description(columns: &[(&str, &LogicalType, i16)], out: &mut OutBuf) {
    let fields: Vec<Field<'_>> = columns
        .iter()
        .map(|(name, logical, format)| {
            let ty = pg_type(logical);
            Field {
                name: name.as_bytes(),
                table: 0,
                column: 0,
                type_oid: ty.oid,
                type_size: TypeInfo::get(ty.oid).map_or(-1, |info| info.len),
                type_modifier: ty.typmod,
                format: *format,
            }
        })
        .collect();
    out.row_description(&fields);
}
