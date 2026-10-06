//! The extended query flow: `Parse`, `Bind`, `Describe`, `Execute` and `Close`.
//!
//! `Parse` prepares the statement in rudb, which parses it but does not bind it, so an error of a
//! table or a column comes at `Describe` or at `Execute` and not at `Parse` as in PostgreSQL. The
//! client sees it before the `Sync` in both cases.
//!
//! `Describe` of a statement binds it without running it, to get the types of the parameters and
//! the columns. `Describe` of a portal of a query or of a change to the data does the same, and
//! the portal runs at `Execute`, as in PostgreSQL. So an error or a notice of the run comes after
//! the `RowDescription`. The statement keeps its description, so the bind at `Describe` is done
//! one time and not for each portal. If the run gives columns of other types than the
//! description, `Execute` fails with the error of PostgreSQL for a plan that changed its result
//! type. `Describe` of a portal of another statement runs the portal and keeps the result for the
//! next `Execute`. An `Execute` with a row limit sends part of the result and `PortalSuspended`,
//! and the next `Execute` continues from there.
//!
//! A parameter in the text format of a declared type is read with the input function of that
//! type. A parameter in the text format with no declared type is a `VARCHAR`, which the binder
//! casts to the type it meets. A parameter in the binary format with no declared type is read as
//! the type that `Describe` finds for it.

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rudb::{Description, Prepared, QueryResult, Transaction};
use rudb_common::{Fields, LogicalType, Origin, Value};
use rudb_pgtypes::{
    DateTimeInput, InputSettings, NoZones, Oid, RowEncoder, TypeError, UNIX_TO_POSTGRES_USECS,
    ZoneAbbrevs, logical_type, param_value, pg_type,
};
use rudb_pgwire::{Bind, CommandTag, Level, OutBuf, Portals, ProtocolError, Statements, Target};

use super::setting::{self, Command};
use super::{
    Control, FLUSH_AT, Failure, Outcome, Runner, column_type, command_tag, field, leading_words,
};

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
        Box::new(fields)
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
    /// The transaction control that the server runs itself, from [`Control::of`].
    control: Option<Control>,
    /// The statement on the settings that the server runs itself, from [`setting::parse`].
    command: Option<Command>,
    /// `None` for an empty query and for a statement on the settings.
    prepared: Option<Prepared>,
    /// The declared type of each parameter, with 0 for no type.
    types: Vec<Oid>,
    /// The parameter of each name of [`Prepared::parameters`], from 0.
    slots: Vec<usize>,
    /// The names are `1` to `n` for `n` parameters, so the values go by position.
    positional: bool,
    /// The types of [`Statement::parameter_types`], found once as PostgreSQL finds them at `Parse`.
    found: OnceLock<Vec<Oid>>,
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

    /// The types of [`Statement::parameter_types`] for the values of `Bind`. A text value of a
    /// parameter of no declared type is read as the type that the binder found, so `1 + $1` adds
    /// two integers.
    fn found(&self) -> Result<&[Oid], Problem> {
        if let Some(types) = self.found.get() {
            return Ok(types);
        }
        let types = self.parameter_types(self.describe()?.as_ref());
        Ok(self.found.get_or_init(|| types))
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
    /// The types and the origins of the columns that a `Describe` sent before the portal ran.
    described: Option<(Vec<LogicalType>, Vec<Option<Origin>>)>,
}

/// A portal that ran, with the place in its result.
struct Ran {
    /// The result of the engine, or `None` for a statement that the server ran itself.
    result: Option<QueryResult>,
    changes: u64,
    tag: CommandTag,
    /// The statement gives rows, so `Execute` sends them.
    rows: bool,
    /// The `CommandComplete` of a statement without rows went out.
    reported: bool,
    chunk: usize,
    row: usize,
    encoder: Option<RowEncoder>,
    /// The error of a query that made rows before it failed. `Execute` sends it after the rows.
    failed: Option<Problem>,
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
        runner: &Runner,
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
        let command = setting::parse(&sql);
        let prepared = match rudb::statements(&sql).map_err(engine)?.len() {
            0 => None,
            1 if command.is_some() => None,
            1 => Some(runner.connection.prepare(&sql).map_err(engine)?),
            _ => {
                return Err(error(
                    "42601",
                    "cannot insert multiple commands into a prepared statement".to_owned(),
                ));
            }
        };
        if (prepared.is_some() || command.is_some())
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
        let control = Control::of(&sql);
        let statement = Statement {
            sql,
            control,
            command,
            prepared,
            types,
            slots,
            positional,
            found: OnceLock::new(),
        };
        // PostgreSQL binds a query and a change to the data at `Parse`, so a name that is not
        // there is an error of `Parse` and not of `Execute`.
        if statement.prepared.as_ref().is_some_and(Prepared::binds_at_parse) {
            statement.describe()?;
        }
        self.statements.insert(name, Arc::new(statement))?;
        out.parse_complete();
        Ok(())
    }

    pub(super) fn bind(
        &mut self,
        runner: &Runner,
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(0));
        let settings = InputSettings {
            datetime: DateTimeInput {
                order: runner.format.date_format.order,
                zone: &runner.zone,
                zones: &NoZones,
                abbrevs: ZoneAbbrevs::postgres_default(),
                now: now + UNIX_TO_POSTGRES_USECS,
            },
            interval_style: runner.format.interval_style,
        };
        let formats = params.formats;
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
                    // A statement that does not bind is read as text here, and `Execute` sends
                    // its error.
                    let mut oid = statement.types[i];
                    if oid == 0 {
                        oid = statement.found().map_or(TEXT, |types| types[i]);
                    }
                    param_value(oid, false, data, i + 1, &settings).map_err(fail)?
                }
                1 => {
                    let mut oid = statement.types[i];
                    if oid == 0 {
                        oid = statement.found()?[i];
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
        // PostgreSQL plans the portal here, and an error of folding a constant comes now.
        if statement.prepared.as_ref().is_some_and(Prepared::binds_at_parse)
            && let Some(error) = statement.describe()?.and_then(|d| d.planning)
        {
            return Err(Problem::failure(Failure::engine(&error, 0), &statement.sql));
        }
        if formats.len() > 1
            && let Some(fields) = statement.describe()?.and_then(|d| d.fields)
        {
            check_formats(&formats, fields.len())?;
        }
        self.portals
            .insert(bind.portal, Portal { statement, values, formats, ran: None, described: None });
        out.bind_complete();
        Ok(())
    }

    pub(super) fn describe(
        &mut self,
        runner: &mut Runner,
        target: Target,
        name: &[u8],
        rest: &[u8],
        out: &mut OutBuf,
    ) -> Result<(), Problem> {
        match target {
            Target::Statement => {
                let statement = self.statements.get(name)?;
                if let Some(command) = &statement.command {
                    out.parameter_description(&[]);
                    describe_command(runner, command, out);
                    return Ok(());
                }
                let description = statement.describe()?;
                out.parameter_description(&statement.parameter_types(description.as_ref()));
                match description {
                    Some(Description { fields: Some(fields), origins, .. }) => {
                        let origin = |at: usize| origins.get(at).copied().flatten();
                        let columns: Vec<_> = fields
                            .iter()
                            .enumerate()
                            .map(|(at, f)| field(&f.name, &f.ty, origin(at), 0))
                            .collect();
                        out.row_description(&columns);
                    }
                    _ => out.no_data(),
                }
            }
            Target::Portal => {
                let portal = self.portals.get_mut(name)?;
                if portal.statement.control.is_some() {
                    out.no_data();
                    return Ok(());
                }
                // PostgreSQL describes a portal from its plan and runs it at `Execute`, so an
                // error of the run and a notice of it come after the `RowDescription`.
                if portal.ran.is_none()
                    && portal.statement.command.is_none()
                    && portal.statement.prepared.as_ref().is_some_and(Prepared::binds_at_parse)
                {
                    let description = portal.statement.describe()?;
                    match description.and_then(|description| {
                        description.fields.map(|f| (f, description.origins))
                    }) {
                        Some((fields, origins)) => {
                            check_formats(&portal.formats, fields.len())?;
                            let origin = |at: usize| origins.get(at).copied().flatten();
                            let columns: Vec<_> = fields
                                .iter()
                                .enumerate()
                                .map(|(at, f)| field(&f.name, &f.ty, origin(at), portal.format(at)))
                                .collect();
                            out.row_description(&columns);
                            let types = fields.into_iter().map(|f| f.ty).collect();
                            portal.described = Some((types, origins));
                        }
                        None => out.no_data(),
                    }
                    return Ok(());
                }
                let formats = portal.formats.clone();
                let alone = alone(rest, name, 1);
                let ran = portal.run(runner, alone, out)?.filter(|ran| ran.rows);
                match ran.and_then(|ran| ran.result.as_ref()) {
                    Some(result) => {
                        let format = |i: usize| match formats.len() {
                            0 => 0,
                            1 => formats[0],
                            _ => formats[i],
                        };
                        let columns: Vec<_> = result
                            .names()
                            .iter()
                            .zip(result.types())
                            .enumerate()
                            .map(|(i, (name, ty))| field(name, ty, result.origin(i), format(i)))
                            .collect();
                        out.row_description(&columns);
                    }
                    _ => out.no_data(),
                }
            }
        }
        Ok(())
    }

    pub(super) fn execute(
        &mut self,
        runner: &mut Runner,
        name: &[u8],
        max_rows: i32,
        rest: &[u8],
        out: &mut OutBuf,
        flush: &mut impl FnMut(&mut OutBuf) -> io::Result<()>,
    ) -> io::Result<Result<(), Problem>> {
        let portal = match self.portals.get_mut(name) {
            Ok(portal) => portal,
            Err(e) => return Ok(Err(e.into())),
        };
        let sql = portal.statement.sql.clone();
        let formats = portal.formats.clone();
        let described = portal.described.take();
        let ran = match portal.run(runner, alone(rest, name, 0), out) {
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
            out.command_tag(ran.tag, ran.changes);
            return Ok(Ok(()));
        }
        let Some(result) = &ran.result else {
            return Ok(Ok(()));
        };
        if let Some((types, origins)) = described {
            let same = types == result.types()
                && (0..types.len())
                    .all(|at| origins.get(at).copied().flatten() == result.origin(at));
            if !same {
                return Ok(Err(error(
                    "0A000",
                    "cached plan must not change result type".to_owned(),
                )));
            }
        }
        if ran.encoder.is_none() {
            let mut columns = Vec::with_capacity(result.width());
            for (i, logical) in result.types().iter().enumerate() {
                let format = match formats.len() {
                    0 => 0,
                    1 => formats[0],
                    _ => formats[i],
                };
                if format != 0 && format != 1 {
                    let message = format!("unsupported format code: {format}");
                    return Ok(Err(error("22023", message)));
                }
                let oid = column_type(logical, result.origin(i)).oid;
                columns.push((logical.clone(), oid, format == 1));
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
        let chunks = result.chunks();
        let mut sent = 0u64;
        while sent < limit && ran.chunk < chunks.len() {
            let chunk = match chunks[ran.chunk].clone().settled() {
                Ok(chunk) => chunk,
                Err(e) => return Ok(Err(Problem::failure(Failure::engine(&e, 0), &sql))),
            };
            let n = chunk.len();
            let take = (n - ran.row).min(usize::try_from(limit - sent).unwrap_or(usize::MAX));
            let rows = ran.row..ran.row + take;
            if let Err(e) = encoder.encode(chunk.columns(), rows, &runner.output(), out.bytes_mut())
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
        } else if let Some(failed) = ran.failed.take() {
            return Ok(Err(failed));
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
    /// The format of the column `at`, from the result formats of `Bind`.
    fn format(&self, at: usize) -> i16 {
        match self.formats.len() {
            0 => 0,
            1 => self.formats[0],
            _ => self.formats[at],
        }
    }

    /// Runs the portal if it did not run yet. `None` is an empty query. The portal runs in the
    /// implicit transaction that ends at `Sync`, but a portal that is `alone` up to `Sync` runs in
    /// its own transaction, which gives the same result for less work.
    fn run(
        &mut self,
        runner: &mut Runner,
        alone: bool,
        out: &mut OutBuf,
    ) -> Result<Option<&mut Ran>, Problem> {
        if self.statement.prepared.is_none() && self.statement.command.is_none() {
            return Ok(None);
        }
        if self.ran.is_none() {
            let statement = &self.statement;
            let sql = &statement.sql;
            let problem = |failure| Problem::failure(failure, sql);
            // A format that is not valid fails after the statement ran.
            if !alone || self.formats.iter().any(|&format| format != 0 && format != 1) {
                runner.begin_implicit().map_err(problem)?;
            }
            let before = runner.connection.transaction();
            let values = &self.values;
            let command = statement.command.as_ref();
            let ran = runner.run(statement.control, command, sql, 0, out, |_| {
                match &statement.prepared {
                    Some(prepared) => statement.execute(prepared, values),
                    None => unreachable!("a statement without a plan is a command"),
                }
            });
            runner.advisory_warnings(out);
            let outcome = match ran {
                Ok(outcome) => outcome,
                Err(failure) => {
                    // The rows that the query made before the error go out first, as in
                    // PostgreSQL, and the error comes where the `CommandComplete` would.
                    let Some(result) = runner.connection.rows_before_error() else {
                        return Err(problem(failure));
                    };
                    let tag = command_tag(sql, &result, before);
                    let mut ran = Ran::new(Some(result), tag, 0, true);
                    ran.failed = Some(problem(failure));
                    self.ran = Some(ran);
                    return Ok(self.ran.as_mut());
                }
            };
            let ran = match outcome {
                Outcome::Result(result) => {
                    let tag = command_tag(sql, &result, before);
                    let changes = result.changes().map_or(0, |n| n as u64);
                    let rows = result.changes().is_none()
                        && (result.width() > 0 || tag == CommandTag::Select);
                    if rows {
                        check_formats(&self.formats, result.width())?;
                    }
                    Ran::new(Some(result), tag, changes, rows)
                }
                Outcome::Done(tag) => Ran::new(None, tag, 0, false),
            };
            self.ran = Some(ran);
        }
        Ok(self.ran.as_mut())
    }
}

impl Ran {
    fn new(result: Option<QueryResult>, tag: CommandTag, changes: u64, rows: bool) -> Ran {
        Ran {
            result,
            changes,
            tag,
            rows,
            reported: false,
            chunk: 0,
            row: 0,
            encoder: None,
            failed: None,
        }
    }
}

/// True when nothing can fail or run between this message and the next `Sync` in `rest`, the
/// messages that the client sent after it: only `Close`, `Flush`, a `Describe` of the same
/// portal and up to `executes` more `Execute` of it. An error in a `Parse` or in a second
/// `Execute` of a portal without rows must roll back the statement. False when the `Sync` did
/// not arrive yet.
fn alone(mut rest: &[u8], portal: &[u8], mut executes: usize) -> bool {
    let same = |body: &[u8]| body.split(|&b| b == 0).next() == Some(portal);
    while rest.len() >= 5 {
        let len = i32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]);
        let Some(end) = usize::try_from(len).ok().map(|len| len + 1).filter(|&end| end >= 5) else {
            return false;
        };
        if end > rest.len() {
            return false;
        }
        let body = &rest[5..end];
        let next = match rest[0] {
            b'S' => return true,
            b'C' | b'H' => true,
            b'D' => matches!(body.split_first(), Some((b'P', name)) if same(name)),
            b'E' if same(body) && executes > 0 => {
                executes -= 1;
                true
            }
            _ => false,
        };
        if !next {
            return false;
        }
        rest = &rest[end..];
    }
    false
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

/// Writes the description of a statement on the settings: the column of `SHOW`, the three columns
/// of `SHOW ALL`, or `NoData`.
fn describe_command(runner: &Runner, command: &Command, out: &mut OutBuf) {
    let text = LogicalType::Varchar;
    match command {
        Command::Show(name) => {
            let column = runner.guc.show(name).map_or_else(|_| name.clone(), |(column, _)| column);
            out.row_description(&[field(&column, &text, None, 0)]);
        }
        Command::ShowAll => {
            let columns =
                ["name", "setting", "description"].map(|name| field(name, &text, None, 0));
            out.row_description(&columns);
        }
        _ => out.no_data(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = vec![tag];
        frame.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    #[test]
    fn a_portal_is_alone_up_to_sync() {
        let execute = frame(b'E', b"p\0\0\0\0\0");
        let sync = frame(b'S', b"");
        let rest = [frame(b'H', b""), frame(b'C', b"Sq\0"), sync.clone()].concat();
        assert!(alone(&rest, b"p", 0));
        assert!(alone(&[execute.clone(), sync.clone()].concat(), b"p", 1));
        assert!(!alone(&[execute.clone(), sync.clone()].concat(), b"p", 0));
        assert!(!alone(&[execute.clone(), sync.clone()].concat(), b"", 1));
        assert!(!alone(&[frame(b'P', b"\0select 1\0\0\0"), sync.clone()].concat(), b"p", 0));
        assert!(!alone(&[frame(b'D', b"Sq\0"), sync.clone()].concat(), b"p", 0));
        assert!(alone(&[frame(b'D', b"Pp\0"), sync.clone()].concat(), b"p", 0));
        // The Sync did not arrive yet.
        assert!(!alone(&execute, b"p", 1));
        assert!(!alone(&sync[..3], b"p", 0));
    }
}
