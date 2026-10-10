//! The server end to end, with a raw client over a Unix socket and over TCP. The messages and
//! their order are those of the PostgreSQL 19 oracle for the same bytes.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use rudb_pgwire::{
    Backend, Bind, Cancel, Frontend, Oids, PROTOCOL_3_0, PROTOCOL_3_2, Packet, Startup, Target,
    encode_oids, encode_options,
};
use rudb_server::{Config, Init, Server, Shutdown, init};

/// A data directory and a socket directory of their own for each test, removed at the end.
struct Dirs {
    root: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rudb-server-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sock")).unwrap();
        init(&root.join("data"), &Init::new("rpg")).unwrap();
        Dirs { root }
    }

    fn config(&self) -> Config {
        let mut config = Config::new(self.root.join("data"));
        config.port = 0;
        config.listen_addresses = "127.0.0.1".to_owned();
        config.unix_socket_directories = self.root.join("sock").display().to_string();
        config
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// One message from the server: the type byte and the body.
#[derive(Debug, Clone)]
struct Message {
    tag: u8,
    body: Vec<u8>,
}

impl Message {
    /// The fields of an `ErrorResponse`, as text.
    fn field(&self, code: u8) -> Option<String> {
        let mut at = 0;
        while at < self.body.len() && self.body[at] != 0 {
            let end = at + 1 + self.body[at + 1..].iter().position(|&b| b == 0).unwrap();
            if self.body[at] == code {
                return Some(String::from_utf8_lossy(&self.body[at + 1..end]).into_owned());
            }
            at = end + 1;
        }
        None
    }

    fn decoded(&self) -> Vec<u8> {
        let mut bytes = vec![self.tag];
        bytes.extend_from_slice(&(self.body.len() as u32 + 4).to_be_bytes());
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

trait Socket: Read + Write {}
impl Socket for UnixStream {}
impl Socket for TcpStream {}

struct Client {
    socket: Box<dyn Socket>,
    input: Vec<u8>,
}

impl Client {
    fn unix(server: &Server) -> Client {
        let socket = UnixStream::connect(&server.sockets()[0]).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Client { socket: Box::new(socket), input: Vec::new() }
    }

    fn tcp(server: &Server) -> Client {
        let socket = TcpStream::connect(server.addresses()[0]).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Client { socket: Box::new(socket), input: Vec::new() }
    }

    fn packet(&mut self, packet: &Packet<'_>) {
        let mut bytes = Vec::new();
        packet.encode(&mut bytes);
        self.socket.write_all(&bytes).unwrap();
    }

    fn startup(&mut self, version: u32, database: &str) {
        self.startup_as(version, "rpg", database);
    }

    fn startup_as(&mut self, version: u32, user: &str, database: &str) {
        let mut options = Vec::new();
        let (user, database) = (user.as_bytes(), database.as_bytes());
        encode_options(
            &[(b"user", user), (b"database", database), (b"application_name", b"t")],
            &mut options,
        );
        self.packet(&Packet::Startup(Startup { version, options: &options }));
    }

    fn send(&mut self, message: &Frontend<'_>) {
        let mut bytes = Vec::new();
        message.encode(&mut bytes);
        self.socket.write_all(&bytes).unwrap();
    }

    fn query(&mut self, sql: &str) -> Vec<Message> {
        self.send(&Frontend::Query(sql.as_bytes()));
        self.until_ready()
    }

    fn parse(&mut self, name: &str, sql: &str, types: &[u32]) {
        let types = encode_oids(types);
        let types = Oids::from_bytes(&types);
        self.send(&Frontend::Parse { name: name.as_bytes(), sql: sql.as_bytes(), types });
    }

    fn bind(&mut self, portal: &str, statement: &str, formats: &[i16], values: &[Option<&[u8]>]) {
        self.bind_with(portal, statement, formats, values, &[]);
    }

    fn bind_with(
        &mut self,
        portal: &str,
        statement: &str,
        formats: &[i16],
        values: &[Option<&[u8]>],
        result_formats: &[i16],
    ) {
        let mut bytes = Vec::new();
        let (portal, statement) = (portal.as_bytes(), statement.as_bytes());
        Bind::encode(&mut bytes, portal, statement, formats, values, result_formats);
        self.socket.write_all(&bytes).unwrap();
    }

    fn describe(&mut self, target: Target, name: &str) {
        self.send(&Frontend::Describe { target, name: name.as_bytes() });
    }

    fn execute(&mut self, portal: &str, max_rows: i32) {
        self.send(&Frontend::Execute { portal: portal.as_bytes(), max_rows });
    }

    fn sync(&mut self) -> Vec<Message> {
        self.send(&Frontend::Sync);
        self.until_ready()
    }

    /// The next message, or `None` at the end of the connection.
    fn next(&mut self) -> Option<Message> {
        loop {
            if self.input.len() >= 5 {
                let len = u32::from_be_bytes(self.input[1..5].try_into().unwrap()) as usize;
                if self.input.len() > len {
                    let body = self.input[5..1 + len].to_vec();
                    let tag = self.input[0];
                    self.input.drain(..1 + len);
                    return Some(Message { tag, body });
                }
            }
            let mut buf = [0u8; 8192];
            let n = self.socket.read(&mut buf).unwrap_or(0);
            if n == 0 {
                return None;
            }
            self.input.extend_from_slice(&buf[..n]);
        }
    }

    fn until_ready(&mut self) -> Vec<Message> {
        let mut messages = Vec::new();
        while let Some(message) = self.next() {
            let done = message.tag == b'Z';
            messages.push(message);
            if done {
                break;
            }
        }
        messages
    }

    /// Every message up to the end of the connection.
    fn rest(&mut self) -> Vec<Message> {
        std::iter::from_fn(|| self.next()).collect()
    }
}

fn tags(messages: &[Message]) -> String {
    messages.iter().map(|m| m.tag as char).collect()
}

fn text(message: &Message) -> String {
    String::from_utf8_lossy(message.body.strip_suffix(b"\0").unwrap_or(&message.body)).into_owned()
}

/// Starts a session and gives its process ID and cancel key.
fn connect(client: &mut Client, version: u32) -> (i32, Vec<u8>) {
    client.startup(version, "postgres");
    let messages = client.until_ready();
    let key = messages.iter().find(|m| m.tag == b'K').unwrap();
    let pid = i32::from_be_bytes(key.body[..4].try_into().unwrap());
    (pid, key.body[4..].to_vec())
}

#[test]
fn the_startup_has_the_messages_of_the_oracle() {
    let dirs = Dirs::new("startup");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    client.startup(PROTOCOL_3_0, "postgres");
    let messages = client.until_ready();
    assert_eq!(tags(&messages), format!("R{}KZ", "S".repeat(15)));
    let names: Vec<String> =
        messages[1..16].iter().map(|m| text(m).split('\0').next().unwrap().to_owned()).collect();
    assert_eq!(
        names,
        [
            "IntervalStyle",
            "search_path",
            "is_superuser",
            "standard_conforming_strings",
            "session_authorization",
            "client_encoding",
            "server_version",
            "server_encoding",
            "in_hot_standby",
            "integer_datetimes",
            "TimeZone",
            "application_name",
            "default_transaction_read_only",
            "scram_iterations",
            "DateStyle",
        ]
    );
    assert_eq!(text(&messages[5]), "session_authorization\0rpg");
    assert_eq!(text(&messages[12]), "application_name\0t");
    assert!(text(&messages[7]).starts_with("server_version\u{0}19.0 (rudb "));
    // Protocol 3.0 has a key of 4 bytes.
    assert_eq!(messages[16].body.len(), 8);
    assert_eq!(messages[17].body, b"I");
    assert_eq!(server.sessions(), 1);
    client.send(&Frontend::Terminate);
    assert!(client.rest().is_empty());
    server.stop().unwrap();
}

#[test]
fn the_simple_query_flow() {
    let dirs = Dirs::new("query");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::tcp(&server);
    connect(&mut client, PROTOCOL_3_2);

    let messages = client.query("select 1 as a, 'x' as b, null as c");
    assert_eq!(tags(&messages), "TDCZ");
    let bytes = messages[0].decoded();
    let Backend::RowDescription(fields) = Backend::decode(&bytes).unwrap().unwrap().0 else {
        panic!("{:?}", messages[0]);
    };
    let shape: Vec<_> = fields.iter().map(|f| (f.name, f.type_oid, f.type_size)).collect();
    assert_eq!(shape, [(&b"a"[..], 23, 4), (&b"b"[..], 25, -1), (&b"c"[..], 25, -1)]);
    let bytes = messages[1].decoded();
    let Backend::DataRow(values) = Backend::decode(&bytes).unwrap().unwrap().0 else {
        panic!("{:?}", messages[1]);
    };
    assert_eq!(values, [Some(&b"1"[..]), Some(&b"x"[..]), None]);
    assert_eq!(text(&messages[2]), "SELECT 1");

    assert_eq!(text(&client.query("create table t (i integer)")[0]), "CREATE TABLE");
    assert_eq!(text(&client.query("insert into t values (1), (2), (3)")[0]), "INSERT 0 3");
    assert_eq!(text(&client.query("update t set i = i + 1 where i > 1")[0]), "UPDATE 2");
    let messages = client.query("select i from t order by i; delete from t");
    assert_eq!(tags(&messages), "TDDDCCZ");
    assert_eq!(text(&messages[4]), "SELECT 3");
    assert_eq!(text(&messages[5]), "DELETE 3");

    // An empty query and a query of only a semicolon have no statement.
    assert_eq!(tags(&client.query("")), "IZ");
    assert_eq!(tags(&client.query(" ; ")), "IZ");

    // A large result goes out in more than one write.
    let messages = client.query("select i from range(100000) r(i)");
    assert_eq!(messages.len(), 100_003);
    assert_eq!(text(&messages[100_001]), "SELECT 100000");
    server.stop().unwrap();
}

#[test]
fn an_error_stops_the_query_and_the_session_goes_on() {
    let dirs = Dirs::new("error");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);

    let messages = client.query("select 1; select nope; select 2");
    assert_eq!(tags(&messages), "TDCEZ");
    let error = &messages[3];
    assert_eq!(error.field(b'S').as_deref(), Some("ERROR"));
    assert_eq!(error.field(b'V').as_deref(), Some("ERROR"));
    // The code is the one of the engine, which uses the DuckDB grammar until milestone PG3.
    assert_eq!(error.field(b'C').map(|c| c.len()), Some(5));
    // The position counts characters in the whole query, from 1.
    assert_eq!(error.field(b'P').as_deref(), Some("18"));
    assert_eq!(messages[4].body, b"I");

    // In a transaction block the status follows the block.
    let messages = client.query("begin");
    assert_eq!((text(&messages[0]).as_str(), messages[1].body.as_slice()), ("BEGIN", &b"T"[..]));
    let messages = client.query("select nope");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[1].body, b"E");
    let messages = client.query("commit");
    assert_eq!((text(&messages[0]).as_str(), messages[1].body.as_slice()), ("ROLLBACK", &b"I"[..]));

    // An error in a message of the extended protocol skips the messages up to the next Sync.
    client.parse("", "select nope from", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42601"));
    assert_eq!(tags(&client.query("select 1")), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn a_string_literal_in_a_cast_uses_the_input_function_of_the_type() {
    let dirs = Dirs::new("literal");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);

    assert_eq!(scalar(&mut client, "select '\\x0102ff'::bytea"), "\\x0102ff");
    assert_eq!(scalar(&mut client, "select 'infinity'::date"), "infinity");
    assert_eq!(scalar(&mut client, "select '4713-01-01 BC'::date"), "4713-01-01 BC");
    assert_eq!(scalar(&mut client, "select '-infinity'::timestamp"), "-infinity");
    assert_eq!(scalar(&mut client, "select '1 day 2 hours'::interval"), "1 day 02:00:00");
    assert_eq!(scalar(&mut client, "select ' 12 '::int4"), "12");

    let messages = client.query("select '2020-02-30'::date");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("22008"));
    let message = messages[0].field(b'M');
    assert_eq!(message.as_deref(), Some("date/time field value out of range: \"2020-02-30\""));
    let messages = client.query("select '99999'::int2");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("22003"));

    // `oid` and `"char"` are the PostgreSQL types, and not the DuckDB types of the same names.
    let messages = client.query("select 4294967295::oid, '\\101'::\"char\"");
    let shape = row_shape(&messages[0]);
    assert_eq!((shape[0].1, shape[1].1), (26, 18));
    assert_eq!(data_row(&messages[1]), [Some(b"4294967295".to_vec()), Some(b"A".to_vec())]);

    // A string literal in the `VALUES` of an `INSERT` is read by the type of its column.
    client.query("create table typed (a oid, b \"char\", d date, x bytea)");
    let messages = client.query("insert into typed values ('8', 'z', 'infinity', '\\x01ff')");
    assert_eq!(tags(&messages), "CZ");
    let messages = client.query("select * from typed");
    let row = data_row(&messages[1]);
    let row: Vec<_> = row.iter().map(|v| String::from_utf8(v.clone().unwrap()).unwrap()).collect();
    assert_eq!(row, ["8", "z", "infinity", "\\x01ff"]);
    let messages = client.query("insert into typed (d) values ('2020-02-30')");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("22008"));
    assert_eq!(messages[0].field(b'P').as_deref(), Some("31"));
    server.stop().unwrap();
}

#[test]
fn a_string_literal_next_to_an_operator_takes_the_type_of_the_other_side() {
    let dirs = Dirs::new("unknown-operand");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values of the PostgreSQL 19 oracle.
    for (sql, value) in [
        ("select 1 + '5'", "6"),
        ("select '5' + 1", "6"),
        ("select 1.5 + '2'", "3.5"),
        ("select '1.5' * 2.0", "3.00"),
        ("select pg_typeof(1::smallint + '3')", "smallint"),
        ("select 2::bigint * '3'", "6"),
        ("select 1 & '3'", "1"),
        ("select interval '1 day' * '2'", "2 days"),
        ("select date '2020-01-01' - '2019-12-01'", "31"),
        ("select 1 || '2'", "12"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    // The literal is read by the input function of the type, and an operator that is not there or
    // that is not unique is refused at the operator.
    for (sql, code, message, place) in [
        ("select 1 + 'x'", "22P02", "invalid input syntax for type integer: \"x\"", "12"),
        ("select 1 + '1.5'", "22P02", "invalid input syntax for type integer: \"1.5\"", "12"),
        (
            "select now() - '1 day'",
            "22007",
            "invalid input syntax for type timestamp with time zone: \"1 day\"",
            "16",
        ),
        ("select '1' + '2'", "42725", "operator is not unique: unknown + unknown", "12"),
        ("select date '2020-01-01' + '1'", "42725", "operator is not unique: date + unknown", "26"),
        ("select 1 + 'abc'::text", "42883", "operator does not exist: integer + text", "10"),
        ("select true + 'a'", "42883", "operator does not exist: boolean + unknown", "13"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(place), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_cast_of_a_string_uses_the_input_function_of_the_type() {
    let dirs = Dirs::new("string-cast");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let of = |text: &str, ty: &str| format!("select t::{ty} from (values ('{text}')) v(t)");
    // The values and the errors of the PostgreSQL 19 oracle. An error of a cast that runs over
    // the rows has no place.
    for (text, ty, value) in [
        (" 12 ", "int", "12"),
        ("0x1F", "int", "31"),
        ("1_000", "int", "1000"),
        ("NaN", "float8", "NaN"),
        ("1.5", "float4", "1.5"),
        ("yes", "bool", "t"),
        ("tru", "bool", "t"),
        ("{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}", "uuid", "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
        ("\\x01ff", "bytea", "\\x01ff"),
        ("4294967295", "oid", "4294967295"),
    ] {
        let sql = of(text, ty);
        assert_eq!(scalar(&mut client, &sql), value, "{sql}");
    }
    assert_eq!(scalar(&mut client, "select cast('7'::varchar as int) + 1"), "8");
    for (sql, code, message) in [
        (of("1.5", "int"), "22P02", "invalid input syntax for type integer: \"1.5\""),
        (of("x", "int"), "22P02", "invalid input syntax for type integer: \"x\""),
        (of("99999", "int2"), "22003", "value \"99999\" is out of range for type smallint"),
        (
            of("9223372036854775808", "int8"),
            "22003",
            "value \"9223372036854775808\" is out of range for type bigint",
        ),
        (of("1e400", "float8"), "22003", "\"1e400\" is out of range for type double precision"),
        (of("x", "bool"), "22P02", "invalid input syntax for type boolean: \"x\""),
        (of("x", "uuid"), "22P02", "invalid input syntax for type uuid: \"x\""),
        ("select 'x'::text::int".into(), "22P02", "invalid input syntax for type integer: \"x\""),
    ] {
        // PostgreSQL folds the cast of a `VALUES` row when it plans, and rudb sends the row
        // description first.
        let messages = client.query(&sql);
        assert!(tags(&messages).ends_with("EZ"), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P'), None, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_cast_between_numbers_checks_the_range_as_postgres_does() {
    let dirs = Dirs::new("number-cast");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values and the errors of the PostgreSQL 19 oracle. A numeric rounds half away from
    // zero and a float rounds half to even.
    for (sql, value) in [
        ("select 3.5::int || ' ' || (-3.5)::int || ' ' || 2.5::int", "4 -4 3"),
        ("select 2.5::float8::int || ' ' || 3.5::float8::int", "2 4"),
        ("select 2.5::float4::int || ' ' || (-2.5)::float8::int2", "2 -2"),
        ("select (-2147483648.4)::float8::int", "-2147483648"),
        ("select 'NaN'::numeric::float4", "NaN"),
        ("select '-Infinity'::float8::float4", "-Infinity"),
        ("select 1e-50::numeric::float8", "1e-50"),
        ("select 32767::int8::int2", "32767"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    let real = format!("\"1{zeros}\" is out of range for type real", zeros = "0".repeat(300));
    let tiny = format!("\"0.{zeros}1\" is out of range for type real", zeros = "0".repeat(49));
    client.query("create table narrow (a int2, b int4, c float4)");
    client.query("insert into narrow (a) values (2.5)");
    for (sql, message) in [
        ("select 2147483648::int", "integer out of range"),
        ("select -32769::int2", "smallint out of range"),
        ("select 70000::int::int2", "smallint out of range"),
        ("select 1e20::numeric::int8", "bigint out of range"),
        ("select 1e10::float8::int", "integer out of range"),
        ("select 'NaN'::float8::int", "integer out of range"),
        ("select 'Infinity'::float8::int8", "bigint out of range"),
        ("select 32767.5::float8::int2", "smallint out of range"),
        ("select 9223372036854775807::int8::float4::int8", "bigint out of range"),
        ("select 1e300::float8::float4", "value out of range: overflow"),
        ("select 1e-300::float8::float4", "value out of range: underflow"),
        ("select 1e300::float4", &real),
        ("select 1e-50::float4", &tiny),
        ("insert into narrow (a) values (70000)", "smallint out of range"),
        ("insert into narrow (a) select 40000::int8", "smallint out of range"),
        ("insert into narrow (b) values (3e10)", "integer out of range"),
        ("insert into narrow (c) values (1e300)", &real),
        ("update narrow set a = 99999", "smallint out of range"),
    ] {
        let messages = client.query(sql);
        assert!(tags(&messages).ends_with("EZ"), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some("22003"), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P'), None, "{sql}");
    }
    assert_eq!(scalar(&mut client, "select a from narrow"), "3");
    server.stop().unwrap();
}

#[test]
fn a_cast_to_text_uses_the_output_function_of_the_type() {
    let dirs = Dirs::new("text-cast");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table floats (a float8, b int[])");
    client.query("insert into floats values ('NaN', array[3, 4]), (1.25, null)");
    // The text of the PostgreSQL 19 oracle, for a cast, for `||` and for a store into a column.
    let checks = |client: &mut Client| {
        for (sql, text) in [
            ("select 'NaN'::float8::text", "NaN"),
            ("select '-Infinity'::float4::text", "-Infinity"),
            ("select 1e20::float8::text || ' ' || 1e-7::float8::text", "1e+20 1e-07"),
            ("select array[1, 2]::text", "{1,2}"),
            ("select array['a b', 'c', null]::text", "{\"a b\",c,NULL}"),
            ("select array['{\"a\"}', 'x\\y', '']::text", "{\"{\\\"a\\\"}\",\"x\\\\y\",\"\"}"),
            ("select array[true, false]::text", "{t,f}"),
            ("select array[1.5::float8, 'NaN'::float8]::text", "{1.5,NaN}"),
            ("select array[]::int[]::text", "{}"),
            ("select 'x' || 'NaN'::float8", "xNaN"),
            ("select 'NaN'::float8 || ' ' || 2", "NaN 2"),
            (
                "select string_agg(a::text || ' ' || coalesce(b::text, '-'), ',' order by a) from floats",
                "1.25 -,NaN {3,4}",
            ),
        ] {
            assert_eq!(scalar(client, sql), text, "{sql}");
        }
    };
    checks(&mut client);
    assert_eq!(scalar(&mut client, "select '\\x01ff'::bytea::text"), "\\x01ff");
    assert_eq!(scalar(&mut client, "select array['\\x01'::bytea]::text"), "{\"\\\\x01\"}");
    client.query("create table texts (t text)");
    client.query("insert into texts select a from floats");
    assert_eq!(scalar(&mut client, "select string_agg(t, ',' order by t) from texts"), "1.25,NaN");
    // The output reads `bytea_output`, and a new value of it makes a new plan.
    client.query("set bytea_output = 'escape'");
    assert_eq!(scalar(&mut client, "select '\\x41ff'::bytea::text"), "A\\377");
    client.query("set bytea_output = 'hex'");
    assert_eq!(scalar(&mut client, "select '\\x41ff'::bytea::text"), "\\x41ff");
    checks(&mut client);
    server.stop().unwrap();
}

#[test]
fn a_string_type_with_a_length_cuts_on_a_cast_and_refuses_a_long_value_on_a_store() {
    let dirs = Dirs::new("length");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);

    // An explicit cast cuts characters and not bytes, and a `name` holds 63 bytes.
    assert_eq!(scalar(&mut client, "select 'abcdef'::varchar(3)"), "abc");
    assert_eq!(scalar(&mut client, "select 'ééé'::varchar(2)"), "éé");
    assert_eq!(scalar(&mut client, "select 'abc'::char"), "a");
    assert_eq!(scalar(&mut client, "select length(repeat('x', 70)::name)"), "63");

    // A store refuses a long value, unless the extra characters are spaces.
    client.query("create table sized (a varchar(3), b char(2), c name)");
    let messages = client.query("insert into sized values ('ab  ', 'x  ', repeat('y', 70))");
    assert_eq!(tags(&messages), "CZ");
    assert_eq!(scalar(&mut client, "select a || '|' || length(c) from sized"), "ab |63");
    for (sql, message) in [
        ("insert into sized (a) values ('abcd')", "value too long for type character varying(3)"),
        ("insert into sized (a) select 'abcd'", "value too long for type character varying(3)"),
        ("update sized set a = 'wxyz'", "value too long for type character varying(3)"),
        ("update sized set b = 'abc'", "value too long for type character(2)"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("22001"), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn an_oid_alias_type_and_a_vector_type_read_and_print_as_in_postgresql() {
    let dirs = Dirs::new("regtype");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);

    assert_eq!(scalar(&mut client, "select 'int4'::regtype"), "integer");
    assert_eq!(scalar(&mut client, "select '_text'::regtype"), "text[]");
    assert_eq!(scalar(&mut client, "select 23::regtype"), "integer");
    assert_eq!(scalar(&mut client, "select '-'::regclass"), "-");
    assert_eq!(scalar(&mut client, "select ' 1  2 3'::int2vector"), "1 2 3");
    assert_eq!(scalar(&mut client, "select '23 25'::oidvector"), "23 25");
    let messages = client.query("select 'int4'::regtype, '1'::int2vector, '1'::oidvector");
    assert_eq!(row_shape(&messages[0]).iter().map(|c| c.1).collect::<Vec<_>>(), [2206, 22, 30]);
    let messages = client.query("select 'nope'::regtype");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42704"));
    assert_eq!(messages[0].field(b'M').as_deref(), Some("type \"nope\" does not exist"));
    server.stop().unwrap();
}

/// `format_type` names a type by its PostgreSQL OID, with the modifier written the way the type
/// writes it, as psql asks for it in `\gdesc` and `\d`. An `oid` from a `VALUES` keeps the function.
#[test]
fn format_type_names_a_type_by_its_oid() {
    let dirs = Dirs::new("formattype");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for (sql, name) in [
        ("select format_type(26, null)", "oid"),
        ("select format_type(1042, null)", "character"),
        ("select format_type(1042, -1)", "bpchar"),
        ("select format_type(1043, 14)", "character varying(10)"),
        ("select format_type(1186, 458751)", "interval year to month"),
        ("select format_type(1007, 5)", "integer[]"),
        ("select format_type(999999, null)", "???"),
        (
            "select string_agg(name || ' ' || pg_catalog.format_type(tp, tpm), ', ') \
             from (values ('a', '16'::pg_catalog.oid, -1), ('b', '1700'::pg_catalog.oid, 655366)) \
             s(name, tp, tpm)",
            "a boolean, b numeric(10,2)",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), name, "{sql}");
    }
    let messages = client.query("select format_type(1186, 0)");
    assert_eq!(messages[0].field(b'M').as_deref(), Some("invalid INTERVAL typmod: 0x0"));
    server.stop().unwrap();
}

/// The row description of a message.
fn row_shape(message: &Message) -> Vec<(String, u32, i16)> {
    let bytes = message.decoded();
    let Backend::RowDescription(fields) = Backend::decode(&bytes).unwrap().unwrap().0 else {
        panic!("{message:?}");
    };
    fields
        .iter()
        .map(|f| (String::from_utf8_lossy(f.name).into_owned(), f.type_oid, f.format))
        .collect()
}

fn parameter_types(message: &Message) -> Vec<u32> {
    let bytes = message.decoded();
    let Backend::ParameterDescription(types) = Backend::decode(&bytes).unwrap().unwrap().0 else {
        panic!("{message:?}");
    };
    types
}

fn data_row(message: &Message) -> Vec<Option<Vec<u8>>> {
    let bytes = message.decoded();
    let Backend::DataRow(values) = Backend::decode(&bytes).unwrap().unwrap().0 else {
        panic!("{message:?}");
    };
    values.iter().map(|v| v.map(<[u8]>::to_vec)).collect()
}

#[test]
fn the_extended_query_flow() {
    let dirs = Dirs::new("extended");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table t (i integer, s varchar)");

    // Describe of a statement gives the types that the binder finds for the parameters.
    client.parse("", "insert into t values ($1, $2)", &[]);
    client.describe(Target::Statement, "");
    let messages = client.sync();
    assert_eq!(tags(&messages), "1tnZ");
    assert_eq!(parameter_types(&messages[1]), [23, 1043]);

    // A text value and a binary value of the type that Describe gives.
    client.bind("", "", &[], &[Some(b"1"), Some(b"a")]);
    client.execute("", 0);
    client.bind("", "", &[1, 0], &[Some(&2i32.to_be_bytes()), Some(b"b")]);
    client.execute("", 0);
    client.bind("", "", &[], &[Some(b"3"), None]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "2C2C2CZ");
    assert_eq!(text(&messages[1]), "INSERT 0 1");

    // A named statement with a declared type, and a portal that sends its rows in two parts.
    client.parse("q", "select i, s from t where i >= $1 order by i", &[23]);
    client.describe(Target::Statement, "q");
    client.bind_with("p", "q", &[], &[Some(b"1")], &[1, 0]);
    client.execute("p", 2);
    client.execute("p", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "1tT2DDsDCZ");
    assert_eq!(parameter_types(&messages[1]), [23]);
    assert_eq!(row_shape(&messages[2]), [("i".to_owned(), 23, 0), ("s".to_owned(), 1043, 0)]);
    assert_eq!(data_row(&messages[4]), [Some(1i32.to_be_bytes().to_vec()), Some(b"a".to_vec())]);
    assert_eq!(data_row(&messages[7]), [Some(3i32.to_be_bytes().to_vec()), None]);
    assert_eq!(text(&messages[8]), "SELECT 1");

    // Describe of a portal gives the formats of the Bind, and a limit that is the number of the
    // rows left suspends the portal as in PostgreSQL.
    client.bind_with("", "q", &[], &[Some(b"2")], &[1]);
    client.describe(Target::Portal, "");
    client.execute("", 2);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "2TDDsCZ");
    assert_eq!(row_shape(&messages[1]), [("i".to_owned(), 23, 1), ("s".to_owned(), 1043, 1)]);
    assert_eq!(text(&messages[5]), "SELECT 0");

    // A parameter of no known type is text.
    client.parse("", "select $1", &[]);
    client.describe(Target::Statement, "");
    let messages = client.sync();
    assert_eq!(tags(&messages), "1tTZ");
    assert_eq!(parameter_types(&messages[1]), [25]);

    // An empty query.
    client.parse("", "", &[]);
    client.describe(Target::Statement, "");
    client.bind("", "", &[], &[]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    assert_eq!(tags(&client.sync()), "1tn2nIZ");

    // Close, and the portals end with the transaction.
    client.bind("p", "q", &[], &[Some(b"1")]);
    client.send(&Frontend::Close { target: Target::Statement, name: b"q" });
    client.execute("p", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "23DDDCZ");
    client.execute("p", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("34000"));
    assert_eq!(messages[0].field(b'M').as_deref(), Some("portal \"p\" does not exist"));
    server.stop().unwrap();
}

#[test]
fn a_portal_is_described_before_it_runs() {
    let dirs = Dirs::new("describe-portal");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The error of the run comes at Execute, after the RowDescription, as in PostgreSQL. A
    // constant `0/0` would fail at Bind, where PostgreSQL folds it.
    client.parse("", "select 1 / g from generate_series(0, 1) g", &[]);
    client.bind("", "", &[], &[]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12TEZ");
    assert_eq!(messages[3].field(b'C').as_deref(), Some("22012"));
    // A described portal sends the rows of the run.
    client.parse("", "select $1::integer + 1 as n", &[]);
    client.bind("", "", &[], &[Some(b"4")]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12TDCZ");
    assert_eq!(row_shape(&messages[2]), [("n".to_owned(), 23, 0)]);
    assert_eq!(data_row(&messages[3]), [Some(b"5".to_vec())]);
    server.stop().unwrap();
}

#[test]
fn the_errors_of_the_extended_query_flow() {
    let dirs = Dirs::new("extended-errors");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::tcp(&server);
    connect(&mut client, PROTOCOL_3_2);
    // The error after any ParseComplete.
    let error = |client: &mut Client| {
        let mut messages = client.sync();
        messages.retain(|m| m.tag != b'1');
        assert_eq!(tags(&messages), "EZ", "{messages:?}");
        let error = &messages[0];
        (error.field(b'C').unwrap(), error.field(b'M').unwrap(), error.field(b'W'))
    };

    client.bind("", "nope", &[], &[]);
    let (code, message, _) = error(&mut client);
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("26000", "prepared statement \"nope\" does not exist")
    );

    client.parse("q", "select $1::integer + 1", &[]);
    client.parse("q", "select 1", &[]);
    client.describe(Target::Statement, "q");
    let messages = client.sync();
    assert_eq!(tags(&messages), "1EZ");
    assert_eq!(messages[1].field(b'C').as_deref(), Some("42P05"));

    client.bind("", "q", &[], &[]);
    let (code, message, _) = error(&mut client);
    assert_eq!(code, "08P01");
    assert_eq!(
        message,
        "bind message supplies 0 parameters, but prepared statement \"q\" requires 1"
    );

    client.parse("", "select $1::integer", &[23]);
    client.bind("", "", &[], &[Some(b"4x")]);
    let (code, message, context) = error(&mut client);
    assert_eq!(code, "22P02");
    assert_eq!(message, "invalid input syntax for type integer: \"4x\"");
    assert_eq!(context.as_deref(), Some("unnamed portal parameter $1"));

    client.parse("", "select $1::integer", &[23]);
    client.bind("p", "", &[1], &[Some(&7i64.to_be_bytes())]);
    let (code, message, context) = error(&mut client);
    assert_eq!(code, "22P03");
    assert_eq!(message, "incorrect binary data format in bind parameter 1");
    assert_eq!(context.as_deref(), Some("portal \"p\" parameter $1"));

    client.parse("", "select $1::integer", &[23]);
    client.bind("", "", &[2], &[Some(b"1")]);
    let (code, message, _) = error(&mut client);
    assert_eq!((code.as_str(), message.as_str()), ("22023", "unsupported format code: 2"));

    client.parse("", "select 1; select 2", &[]);
    let (code, message, _) = error(&mut client);
    assert_eq!(code, "42601");
    assert_eq!(message, "cannot insert multiple commands into a prepared statement");

    client.parse("", "select 1, 2", &[]);
    client.bind_with("", "", &[], &[], &[0, 1, 0]);
    client.execute("", 0);
    let (code, message, _) = error(&mut client);
    assert_eq!(code, "08P01");
    assert_eq!(message, "bind message has 3 result formats but query has 2 columns");

    client.parse("", "select 1", &[]);
    client.bind_with("", "", &[], &[], &[2]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12EZ");
    assert_eq!(messages[2].field(b'C').as_deref(), Some("22023"));

    // A portal of a statement without rows runs one time.
    client.query("create table t (i integer)");
    client.parse("", "insert into t values (1)", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12CEZ");
    assert_eq!(messages[3].field(b'C').as_deref(), Some("55000"));
    assert_eq!(messages[3].field(b'M').as_deref(), Some("portal \"\" cannot be run"));

    // In a failed transaction a Parse gives 25P02, except for a statement that ends it.
    client.query("begin");
    client.query("select nope");
    client.parse("", "select 1", &[]);
    client.bind("", "", &[], &[]);
    let messages = client.sync();
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("25P02"));
    assert_eq!(messages[1].body, b"E");
    client.parse("", "rollback", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12CZ");
    assert_eq!(messages[3].body, b"I");

    // An error goes to the client at once, also in a pipeline that sends Flush and no Sync.
    client.parse("", "select nope(1)", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    client.send(&Frontend::Flush);
    let error = client.next().unwrap();
    assert_eq!((error.tag, error.field(b'C').as_deref()), (b'E', Some("42883")));
    assert_eq!(tags(&client.sync()), "Z");
    server.stop().unwrap();
}

#[test]
fn a_name_that_if_exists_lets_go_gives_a_notice() {
    let dirs = Dirs::new("skipping");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let notices = |messages: &[Message]| -> Vec<(String, String)> {
        let notices = messages.iter().filter(|message| message.tag == b'N');
        notices
            .map(|message| {
                assert_eq!(message.field(b'S').as_deref(), Some("NOTICE"));
                (message.field(b'C').unwrap(), message.field(b'M').unwrap())
            })
            .collect()
    };
    let skipping = |kind: &str, name: &str| {
        ("00000".to_owned(), format!("{kind} \"{name}\" does not exist, skipping"))
    };

    // One notice for each name, before the tag.
    let messages = client.query("drop table if exists nx, ny");
    assert_eq!(tags(&messages), "NNCZ");
    assert_eq!(notices(&messages), [skipping("table", "nx"), skipping("table", "ny")]);
    assert_eq!(notices(&client.query("drop view if exists nv")), [skipping("view", "nv")]);
    assert_eq!(notices(&client.query("drop index if exists ni")), [skipping("index", "ni")]);
    assert_eq!(notices(&client.query("drop schema if exists ns")), [skipping("schema", "ns")]);

    // A name that is there gives a notice for a create that does nothing.
    client.query("create table t (a integer)");
    client.query("create index i on t (a)");
    let exists =
        |name: &str| ("42P07".to_owned(), format!("relation \"{name}\" already exists, skipping"));
    assert_eq!(notices(&client.query("create table if not exists t (a integer)")), [exists("t")]);
    assert_eq!(notices(&client.query("create index if not exists i on t (a)")), [exists("i")]);
    let messages = client.query("drop table if exists t");
    assert_eq!(tags(&messages), "CZ");

    // client_min_messages above NOTICE keeps them from the client, and ERROR keeps warnings too.
    client.query("set client_min_messages = warning");
    assert_eq!(tags(&client.query("drop table if exists nx")), "CZ");
    // The statements that the server runs without the engine keep the same rule, for example the
    // first statements of pg_regress.
    assert_eq!(tags(&client.query("drop database if exists nd")), "CZ");
    assert_eq!(tags(&client.query("drop role if exists nr")), "CZ");
    assert_eq!(tags(&client.query("commit")), "NCZ");
    client.query("set client_min_messages = error");
    assert_eq!(tags(&client.query("commit")), "CZ");
    client.query("reset client_min_messages");
    assert_eq!(tags(&client.query("drop table if exists nx")), "NCZ");
    assert_eq!(notices(&client.query("drop database if exists nd")), [skipping("database", "nd")]);
    server.stop().unwrap();
}

#[test]
fn a_notice_of_the_parse_comes_before_the_rows_as_postgres_does_it() {
    let dirs = Dirs::new("parse-notices");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let long = "a".repeat(70);
    let cut = format!("identifier \"{long}\" will be truncated to \"{}\"", &long[..63]);
    let notices = |messages: &[Message]| -> Vec<String> {
        let notices = messages.iter().filter(|message| message.tag == b'N');
        notices
            .map(|message| {
                assert_eq!(message.field(b'S').as_deref(), Some("NOTICE"));
                assert_eq!(message.field(b'C').as_deref(), Some("42622"));
                message.field(b'M').unwrap()
            })
            .collect()
    };

    // The notice comes before the rows, and again when the same statement runs again.
    for _ in 0..2 {
        let messages = client.query(&format!("select 1 as {long}"));
        assert_eq!(tags(&messages), "NTDCZ");
        assert_eq!(notices(&messages), std::slice::from_ref(&cut));
    }

    // The notices come in the order of the text, and before an error.
    let messages = client.query(&format!("select {long} from (select 1 as b{long}) s"));
    assert_eq!(tags(&messages), "NNEZ");
    assert_eq!(messages[2].field(b'C').as_deref(), Some("42703"));
    let messages = client.query(&format!("select 1 as {long}; select 2 as {long}"));
    assert_eq!(tags(&messages), "NTDCNTDCZ");

    // A statement that streams its rows and a statement that changes the catalog.
    let messages = client.query(&format!("select count(*) as {long} from generate_series(1, 3)"));
    assert_eq!(tags(&messages), "NTDCZ");
    assert_eq!(tags(&client.query(&format!("create table {long} (a integer)"))), "NCZ");
    assert_eq!(tags(&client.query(&format!("drop table {long}"))), "NCZ");

    // Parse gives the notice once, before ParseComplete, and Execute does not give it again.
    client.parse("", &format!("select 1 as {long}"), &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "N12DCZ");
    assert_eq!(notices(&messages), std::slice::from_ref(&cut));

    // client_min_messages above NOTICE keeps them from the client.
    client.query("set client_min_messages = warning");
    assert_eq!(tags(&client.query(&format!("select 1 as {long}"))), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn a_parameter_of_no_type_takes_the_type_that_postgres_gives_it() {
    let dirs = Dirs::new("inference");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table t (a int4, c text, h varchar(5), i timestamptz)");
    let cases: [(&str, &[u32]); 13] = [
        ("select a from t limit $1 offset $2", &[20, 20]),
        ("insert into t (a) select $1", &[23]),
        ("select a from t union select $1", &[23]),
        ("select $1 union select $2", &[25, 25]),
        ("select a from t where a = $1 limit $2", &[23, 20]),
        ("insert into t (h, a) values ($1, $2)", &[1043, 23]),
        ("update t set h = $1", &[1043]),
        ("select $1::varchar(3), $2::char(2)", &[1043, 1042]),
        ("select a from t where (a, c) = ($1, $2)", &[23, 25]),
        ("select now() + $1, now() - $2", &[1186, 1184]),
        ("select current_date - $1, interval '1 day' / $2", &[1082, 701]),
        ("select a from t where i > now() - $1::interval", &[1186]),
        ("select '10:00'::time - $1", &[1083]),
    ];
    for (sql, types) in cases {
        client.parse("", sql, &[]);
        client.describe(Target::Statement, "");
        let messages = client.sync();
        assert_eq!(tags(&messages)[..2], *"1t", "{sql}");
        assert_eq!(parameter_types(&messages[1]), types, "{sql}");
    }
    // A parameter that the query does not use has no type, unless Parse gives it one.
    for (sql, missing) in [("select $2", "$1"), ("select $3, $1", "$2")] {
        client.parse("", sql, &[]);
        let messages = client.sync();
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42P18"), "{sql}");
        let message = format!("could not determine data type of parameter {missing}");
        assert_eq!(messages[0].field(b'M'), Some(message), "{sql}");
    }
    client.parse("", "select $2", &[23]);
    client.describe(Target::Statement, "");
    let messages = client.sync();
    assert_eq!(parameter_types(&messages[1]), [23, 25]);
    server.stop().unwrap();
}

#[test]
fn division_follows_the_rules_of_postgres() {
    let dirs = Dirs::new("division");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query("select 7 / 2, -7 / 2, 7::float8 / 2");
    let row: Vec<_> = data_row(&messages[1]).into_iter().map(Option::unwrap).collect();
    assert_eq!(row, [b"3".to_vec(), b"-3".to_vec(), b"3.5".to_vec()]);
    // An interval divides with `/`, two times give an interval and two dates an `int4`.
    let sql = "select interval '1 mon 1 day' / 7, time '09:00' - time '23:59', \
               date '2020-03-01' - date '2020-02-01'";
    let messages = client.query(sql);
    assert_eq!(row_shape(&messages[0]).iter().map(|c| c.1).collect::<Vec<_>>(), [1186, 1186, 23]);
    let row: Vec<_> = data_row(&messages[1]).into_iter().map(Option::unwrap).collect();
    assert_eq!(row, [b"4 days 10:17:08.546743".to_vec(), b"-14:59:00".to_vec(), b"29".to_vec()]);
    // A zero divisor is an error for every type, with no position, as it is in PostgreSQL.
    for sql in ["select 0 / 0", "select 1 % 0", "select 1.0 / 0", "select 1::float8 / 0"] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("22012"), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some("division by zero"), "{sql}");
        assert_eq!(messages[0].field(b'P'), None, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn any_and_all_compare_against_the_elements_of_an_array() {
    let dirs = Dirs::new("quantified");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or("null".to_string(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join(",")
    };
    // A null answer makes the whole answer null only when no element settles it, and an empty
    // array settles it for any left side, a null one too.
    let cases = [
        ("select 1 = any(array[1,2]), 3 <> all(array[1,2]), 1 = any('{1,2}')", "t,t,t"),
        (
            "select 1 = any(array[null,2]), 2 = any(array[null,2]), 3 < all(array[2,null])",
            "null,t,f",
        ),
        ("select null::int = any(array[]::int[]), null::int = all(array[]::int[])", "f,t"),
        ("select 1 = any(null::int[]), null::int = any(array[1])", "null,null"),
        (
            "select 4 = any(array[[1,2],[3,4]]), 2 >= all(array(select generate_series(1, 2)))",
            "t,t",
        ),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(text(data_row(&messages[1])), expected, "{sql}");
    }
    let messages = client.query("select 1 = any(5)");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42809"));
    // A string on the left is read as the element type.
    let messages = client.query("select 'a' = any(array[1,2])");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("22P02"));
    // The parameter is an array of the type on the left.
    client.query("create table t (a int4, c text)");
    for (sql, types) in [
        ("select a from t where a = any($1)", [1007]),
        ("select a from t where c <> all($1)", [1009]),
        ("select a from t where a = any($1::int8[])", [1016]),
    ] {
        client.parse("", sql, &[]);
        client.describe(Target::Statement, "");
        let messages = client.sync();
        assert_eq!(parameter_types(&messages[1]), types, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_char_column_pads_on_output_and_ignores_trailing_spaces() {
    let dirs = Dirs::new("bpchar");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or("null".to_string(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join(",")
    };
    client.query("create table t (c char(4) primary key, v varchar(6))");
    client.query("insert into t values ('ab', 'ab  '), ('a b  ', 'x')");
    // The value goes out padded to 4 characters, and the trailing spaces count nowhere else.
    let cases = [
        ("select c, length(c), c || '|', c::text = 'ab' from t where v = 'ab  '", "ab  ,2,ab|,t"),
        ("select c = v, c = 'ab    ', c::text = v from t where v = 'ab  '", "t,t,f"),
        ("select count(*) from t where c in ('ab ', 'a b')", "2"),
        ("select 'ab'::char(4), 'ab'::char(4) = 'ab  '::char(4), 'abcdef'::char(3)", "ab  ,t,abc"),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(text(data_row(&messages[1])), expected, "{sql}");
    }
    let messages = client.query("insert into t values ('abcde', 'y')");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("22001"));
    // A parameter compared with the column is a `bpchar`, and a stored parameter keeps the
    // length rule of the column.
    client.parse("", "select c from t where c = $1", &[]);
    client.describe(Target::Statement, "");
    client.bind("", "", &[], &[Some(b"ab  ")]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(parameter_types(&messages[1]), [1042]);
    assert_eq!(text(data_row(&messages[4])), "ab  ");
    client.parse("", "insert into t values ($1, $2)", &[]);
    client.bind("", "", &[], &[Some(b"q  "), Some(b"q")]);
    client.execute("", 0);
    assert_eq!(tags(&client.sync()), "12CZ");
    client.bind("", "", &[], &[Some(b"qqqqq"), Some(b"q")]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "2EZ");
    assert_eq!(messages[1].field(b'C').as_deref(), Some("22001"));
    let messages = client.query("select length(c), c from t where c = 'q'");
    assert_eq!(text(data_row(&messages[1])), "1,q   ");
    server.stop().unwrap();
}

#[test]
fn the_functions_of_postgres_have_its_result_types() {
    let dirs = Dirs::new("pgcalls");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or("null".to_string(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join(",")
    };
    // Each case has the values and the type OIDs of PostgreSQL.
    let cases = [
        (
            "select length('h\u{e9}llo'), octet_length('h\u{e9}llo'), length('ab'::bytea), strpos('abc', 'c')",
            "5,6,2,3",
            vec![23, 23, 23, 23],
        ),
        (
            "select cardinality(array[[1,2],[3,4]]), array_ndims(array[[1],[2]]), \
             array_length(array[[1,2,3]], 2), array_upper(array[1], 2), array_lower(array[]::int[], 1)",
            "4,2,3,null,null",
            vec![23, 23, 23, 23, 23],
        ),
        (
            "select num_nulls(1, null, 2), num_nonnulls(1, null), width_bucket(5.35, 0.024, 10.06, 5), \
             regexp_count('abcabc', 'b'), regexp_instr('abcabc', 'c')",
            "1,1,3,2,3",
            vec![23, 23, 23, 23, 23],
        ),
        (
            "select sum(x), sum(x::int8), every(x > 1) from (values (2), (4)) t(x)",
            "6,6,t",
            vec![20, 1700, 16],
        ),
        ("select gcd(4, 6), gcd(4::int8, 6)", "2,2", vec![23, 20]),
        ("select date_part('second', timestamp '2024-05-01 10:00:01.5')", "1.5", vec![701]),
        (
            "select sign(-2.5), sign(3), sum(g) from generate_series(1, 4) g",
            "-1,1,10",
            vec![1700, 701, 20],
        ),
        (
            "select round(x, 2), trunc(x, 1), ceil(x), floor(x), abs(x), round(5::int8, 1), round(5) \
             from (values (-123456789012345678901.555::numeric)) t(x)",
            "-123456789012345678901.56,-123456789012345678901.5,-123456789012345678901,\
             -123456789012345678902,123456789012345678901.555,5.0,5",
            vec![1700, 1700, 1700, 1700, 1700, 1700, 701],
        ),
        (
            "select var_pop(x), var_samp(x), stddev_pop(x), stddev_samp(x) \
             from (values (1), (2), (4)) t(x)",
            "1.5555555555555556,2.3333333333333333,1.2472191289246471,1.5275252316519467",
            vec![1700, 1700, 1700, 1700],
        ),
        (
            "select variance(x::float8), stddev(x::float4), var_samp(7) \
             from (values (1.5), (2.25), (4)) t(x)",
            "1.6458333333333333,1.282900359861721,0",
            vec![701, 701, 1700],
        ),
        (
            "select var_pop(x) over (order by x rows 1 preceding), stddev(x::float8) over () \
             from (values (4), (1)) t(x)",
            "0,2.1213203435596424",
            vec![1700, 701],
        ),
        (
            "select ntile(2) over (order by x), lag(x, 1, 0.5) over (order by x) \
             from (values (1), (2)) t(x)",
            "1,0.5",
            vec![23, 1700],
        ),
    ];
    for (sql, expected, oids) in cases {
        let messages = client.query(sql);
        let shape: Vec<u32> = row_shape(&messages[0]).into_iter().map(|(_, oid, _)| oid).collect();
        assert_eq!(shape, oids, "{sql}");
        assert_eq!(text(data_row(&messages[1])), expected, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_window_or_an_aggregate_where_postgres_refuses_one_has_its_error() {
    let dirs = Dirs::new("misplaced");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create temp table t (a int)");
    // The texts, the codes and the places are the ones that PostgreSQL 19 gives.
    for (sql, code, message, place) in [
        (
            "delete from t where rank() over () > 1",
            "42P20",
            "window functions are not allowed in WHERE",
            "21",
        ),
        (
            "delete from t where sum(a) > 1",
            "42803",
            "aggregate functions are not allowed in WHERE",
            "21",
        ),
        (
            "update t set a = rank() over ()",
            "42P20",
            "window functions are not allowed in UPDATE",
            "18",
        ),
        ("update t set a = sum(a)", "42803", "aggregate functions are not allowed in UPDATE", "18"),
        (
            "delete from t returning rank() over ()",
            "42P20",
            "window functions are not allowed in RETURNING",
            "25",
        ),
        (
            "insert into t values (1) returning sum(a)",
            "42803",
            "aggregate functions are not allowed in RETURNING",
            "36",
        ),
        (
            "select a from t group by a having rank() over () > 1",
            "42P20",
            "window functions are not allowed in HAVING",
            "35",
        ),
        (
            "select a from t offset rank() over ()",
            "42P20",
            "window functions are not allowed in OFFSET",
            "24",
        ),
        (
            "select a from t join t s on rank() over () = 1",
            "42P20",
            "window functions are not allowed in JOIN conditions",
            "29",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(place), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_ordered_set_aggregates_of_postgres_give_its_values_and_its_errors() {
    let dirs = Dirs::new("orderedset");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or("null".to_string(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The values and the type OIDs are the ones that PostgreSQL 19 gives.
    let cases = [
        (
            "select percentile_cont(0.1) within group (order by x), \
             percentile_disc(0.25) within group (order by x desc), \
             mode() within group (order by x % 3) from generate_series(1, 5) x",
            "1.4|4|1",
            vec![701, 23, 23],
        ),
        (
            "select percentile_cont(array[0, 0.25, 1]) within group (order by x), \
             percentile_disc(array[null, 0.5]) within group (order by x::text) \
             from generate_series(1, 6) x",
            "{1,2.25,6}|{NULL,3}",
            vec![1022, 1009],
        ),
        (
            "select rank(3) within group (order by x), dense_rank(3) within group (order by x), \
             percent_rank(3) within group (order by x), cume_dist(3) within group (order by x) \
             from (values (1), (1), (2), (2), (3), (3), (4)) v(x)",
            "5|3|0.5714285714285714|0.875",
            vec![20, 20, 701, 701],
        ),
        (
            "select percentile_cont(0.5) within group (order by x) \
             from (values (interval '1 day'), (interval '2 days 3 hours')) v(x)",
            "1 day 13:30:00",
            vec![1186],
        ),
    ];
    for (sql, expected, oids) in cases {
        let messages = client.query(sql);
        let shape: Vec<u32> = row_shape(&messages[0]).into_iter().map(|(_, oid, _)| oid).collect();
        assert_eq!(shape, oids, "{sql}");
        assert_eq!(text(data_row(&messages[1])), expected, "{sql}");
    }
    for (sql, code, message, place) in [
        (
            "select percentile_cont(1.5) within group (order by x) from generate_series(1, 3) x",
            "22003",
            "percentile value 1.5 is not between 0 and 1",
            None,
        ),
        (
            "select sum() within group (order by x::float8) from generate_series(1, 3) x",
            "42809",
            "sum is not an ordered-set aggregate, so it cannot have WITHIN GROUP",
            Some("8"),
        ),
        (
            "select percentile_cont(0.5, 0.5) from generate_series(1, 3) x",
            "42809",
            "WITHIN GROUP is required for ordered-set aggregate percentile_cont",
            Some("8"),
        ),
        (
            "select rank(x) within group (order by x) from generate_series(1, 5) x",
            "42803",
            "column \"x.x\" must appear in the GROUP BY clause or be used in an aggregate function",
            Some("13"),
        ),
        (
            "select rank(3) within group (order by x) from (values ('fred'), ('jim')) v(x)",
            "42804",
            "WITHIN GROUP types text and integer cannot be matched",
            Some("13"),
        ),
        (
            "select rank(3) within group (order by x, x) from generate_series(1, 5) x",
            "42883",
            "function rank(integer, integer, integer) does not exist",
            Some("8"),
        ),
        (
            "select percentile_cont(0.5) within group (order by x) over () \
             from generate_series(1, 3) x",
            "0A000",
            "OVER is not supported for ordered-set aggregate percentile_cont",
            Some("8"),
        ),
    ] {
        // A percentile out of range is found as the group finishes, after the row description.
        let messages = client.query(sql);
        assert!(tags(&messages).ends_with("EZ"), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P').as_deref(), place, "{sql}");
    }
    server.stop().unwrap();
}

/// A call whose `OVER` or `RESPECT NULLS` or `IGNORE NULLS` does not fit the function is refused
/// with the errors of PostgreSQL 19, once the function is found.
#[test]
fn a_window_or_a_null_treatment_on_the_wrong_function_is_refused_as_postgres_refuses_it() {
    let dirs = Dirs::new("pgovercall");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query(
        "select string_agg(coalesce(l::text, '-'), ',' order by i) from (select i, \
         lag(v) ignore nulls over (order by i) l, first_value(v) respect nulls over (order by i) f \
         from (values (1, 1), (2, null), (3, 3), (4, null)) t(i, v)) s",
    );
    assert_eq!(data_row(&messages[1]), vec![Some(b"-,1,1,3".to_vec())]);
    for (sql, code, message, place) in [
        (
            "select abs(1) ignore nulls",
            "42809",
            "RESPECT/IGNORE NULLS specified, but abs is not a window function",
            Some("8"),
        ),
        (
            "select sum(x) respect nulls from generate_series(1, 3) x",
            "42809",
            "aggregate functions do not accept RESPECT/IGNORE NULLS",
            Some("8"),
        ),
        (
            "select sum(x) ignore nulls over () from generate_series(1, 3) x",
            "42809",
            "aggregate functions do not accept RESPECT/IGNORE NULLS",
            Some("8"),
        ),
        (
            "select row_number() respect nulls over () from generate_series(1, 3) x",
            "0A000",
            "function row_number does not allow RESPECT/IGNORE NULLS",
            None,
        ),
        (
            "select first_value(x) ignore nulls from generate_series(1, 3) x",
            "42809",
            "window function first_value requires an OVER clause",
            Some("8"),
        ),
        (
            "select abs(x) over () from generate_series(1, 3) x",
            "42809",
            "OVER specified, but abs is not a window function nor an aggregate function",
            Some("8"),
        ),
        (
            "select lower(x) over () from generate_series(1, 3) x",
            "42883",
            "function lower(integer) does not exist",
            Some("8"),
        ),
        (
            "select nosuch(x) ignore nulls over () from generate_series(1, 3) x",
            "42883",
            "function nosuch(integer) does not exist",
            Some("8"),
        ),
        (
            "select count() over () from generate_series(1, 3) x",
            "42809",
            "count(*) must be used to call a parameterless aggregate function",
            Some("8"),
        ),
        (
            "select nth_value(x, 0) over () from generate_series(1, 3) x",
            "22016",
            "argument of nth_value must be greater than zero",
            None,
        ),
    ] {
        let messages = client.query(sql);
        assert!(tags(&messages).ends_with("EZ"), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P').as_deref(), place, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_math_functions_of_postgres_give_its_values_and_its_errors() {
    let dirs = Dirs::new("pgmath");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or("null".to_string(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The values are the ones that PostgreSQL 19 gives.
    for (sql, expected) in [
        (
            "select sign(-8.4), sign(0.0), sign('NaN'::numeric), sign('-Infinity'::numeric)",
            "-1|0|NaN|-1",
        ),
        (
            "select scale(8.4100), scale('NaN'::numeric), min_scale(8.4100), min_scale(0.00), \
             trim_scale(8.4100), trim_scale(100.000), trim_scale(0.000)",
            "4|null|2|0|8.41|100|0",
        ),
        (
            "select div(9.5, 2), div(-9.5, 2), div(1, 'Infinity'::numeric), \
             div('Infinity'::numeric, -2), div('NaN'::numeric, 1)",
            "4|-4|0|-Infinity|NaN",
        ),
        (
            "select factorial(0), factorial(20), factorial(25)",
            "1|2432902008176640000|15511210043330985984000000",
        ),
        (
            "select gcd(1.5, 0.25), lcm(1.5, 0.25), gcd(-12.0, 18), lcm(0, 5.5), \
             gcd('NaN'::numeric, 1)",
            "0.25|1.50|6.0|0.0|NaN",
        ),
        (
            "select width_bucket(5.35, 0.024, 10.06, 5), width_bucket(-1.0, 0, 10, 5), \
             width_bucket(10.0, 0, 10, 5), width_bucket(5.0, 10, 0, 5), \
             width_bucket('NaN'::numeric, 0, 1, 3)",
            "3|0|6|3|4",
        ),
        (
            "select width_bucket(5.35::float8, 0.024, 10.06, 5), \
             width_bucket(1e308::float8, -1e308, 1e308, 10), width_bucket(0::float8, 10, 0, 4)",
            "3|11|5",
        ),
        (
            "select sind(30), cosd(60), tand(45), cotd(45), asind(0.5), acosd(0.5), atand(1), \
             atan2d(1, 1), sind(-90), tand(90)",
            "0.5|0.5|1|1|30|60|45|45|-1|Infinity",
        ),
        (
            "select mod(-2147483648, -1), to_bin(-1), to_oct(-8::int8), to_hex(-1::int8)",
            "0|11111111111111111111111111111111|1777777777777777777770|ffffffffffffffff",
        ),
        ("select erf(0.5), erfc(0.5), gamma(5)", "0.5204998778130465|0.4795001221869535|24"),
        (
            "select round(-2.5), round(2.5::float8), round(3.5::float8), round(1234.5, -2)",
            "-3|2|4|1200",
        ),
        (
            "select sqrt(2.0), sqrt(1e-20::numeric), sqrt(123456789012345678901234567890::numeric)",
            "1.414213562373095|0.0000000001000000000000000|351364182882014.4",
        ),
        (
            "select exp(1.0), exp(-1.0), exp(10.5), exp(100::numeric)",
            "2.7182818284590452|0.3678794411714423|36315.502674246638|\
             26881171418161354484126255515800135873611119",
        ),
        (
            "select ln(2.0), ln(10::numeric), ln(1e100::numeric), ln(1.000000000001)",
            "0.6931471805599453|2.3025850929940457|230.25850929940457|\
             0.0000000000009999999999995000",
        ),
        (
            "select log(100.0), pg_typeof(log(100.0)), log(3.0, 7.5), log(2.0, 1e100)",
            "2.0000000000000000|numeric|1.8340437671464697|332.19280948873623",
        ),
        (
            "select power(2.0, 10.0), power(2.0, 0.5), power(-8.0, 3.0), power(10.0, -2.0), \
             power(1.5, 100), power(1.000001, 1000000)",
            "1024.0000000000000|1.4142135623730950|-512.00000000000000|0.010000000000000000|\
             406561177535215237.4|2.7182804693193769",
        ),
        (
            "select power('NaN'::numeric, 0), power('-Infinity'::numeric, 3), \
             power(0.5, 'Infinity'::numeric), power(-2, 'Infinity'::numeric)",
            "1|-Infinity|0|Infinity",
        ),
        (
            "select 'NaN'::float8 / 0, 'Infinity'::float8 * 2, 0::float8 * 'Infinity', \
             1::float8 / 'Infinity', 1.5::float4 * 2::float4, pg_typeof(1.5::float4 * 2::float4)",
            "NaN|Infinity|NaN|0|3|real",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "TDCZ", "{sql}");
        assert_eq!(text(data_row(&messages[1])), expected, "{sql}");
    }
    for (sql, state, message) in [
        ("select div(1.0, 0)", "22012", "division by zero"),
        ("select factorial(-1)", "22003", "factorial of a negative number is undefined"),
        ("select width_bucket(1.0, 0, 10, 0)", "2201G", "count must be greater than zero"),
        ("select width_bucket(1.0, 1, 1, 3)", "2201G", "lower bound cannot equal upper bound"),
        ("select dexp(800)", "22003", "value out of range: overflow"),
        ("select exp(-1000::float8)", "22003", "value out of range: underflow"),
        ("select gcd(-2147483648, 0)", "22003", "integer out of range"),
        ("select lcm(2147483647, 2147483646)", "22003", "integer out of range"),
        ("select ln(0::float8)", "2201E", "cannot take logarithm of zero"),
        ("select log10(-1::float8)", "2201E", "cannot take logarithm of a negative number"),
        ("select sqrt(-1)", "2201F", "cannot take square root of a negative number"),
        ("select acos(2)", "22003", "input is out of range"),
        ("select sqrt(-1.0)", "2201F", "cannot take square root of a negative number"),
        ("select exp(6000.0)", "22003", "value overflows numeric format"),
        ("select ln(0.0)", "2201E", "cannot take logarithm of zero"),
        ("select log(1.0, 10.0)", "22012", "division by zero"),
        ("select log(-2.0, 10.0)", "2201E", "cannot take logarithm of a negative number"),
        (
            "select power(-8.0, 0.5)",
            "2201F",
            "a negative number raised to a non-integer power yields a complex result",
        ),
        ("select power(0.0, -1.0)", "2201F", "zero raised to a negative power is undefined"),
        ("select 2147483647 + 1", "22003", "integer out of range"),
        ("select 9223372036854775807 + 1", "22003", "bigint out of range"),
        ("select 32767::int2 * 2::int2", "22003", "smallint out of range"),
        ("select -(-2147483647 - 1)", "22003", "integer out of range"),
        ("select abs(-2147483647 - 1)", "22003", "integer out of range"),
        ("select 1e308::float8 * 10", "22003", "value out of range: overflow"),
        ("select -1e308::float8 - 1e308::float8", "22003", "value out of range: overflow"),
        ("select 1e-308::float8 * 1e-308::float8", "22003", "value out of range: underflow"),
        ("select 1e-308::float8 / 1e308::float8", "22003", "value out of range: underflow"),
        ("select 1.5::float8 / 0", "22012", "division by zero"),
        ("select 3e38::float4 * 10::float4", "22003", "value out of range: overflow"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
    }
    // The first row is sent, and the second is past the range.
    for (sql, message) in [
        ("select x * 10 from (values (1.0::float8), (1e308)) t(x)", "value out of range: overflow"),
        ("select x + 1 from (values (1), (2147483647)) t(x)", "integer out of range"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "TDEZ", "{sql}");
        assert_eq!(messages[2].field(b'C').as_deref(), Some("22003"), "{sql}");
        assert_eq!(messages[2].field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_call_of_the_function_of_an_operator_is_the_operator() {
    let dirs = Dirs::new("pgopfunc");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or("null".to_string(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The values and the types are the ones that PostgreSQL 19 gives. The shifts shift as C
    // does, with the count modulo the width of the type that C shifts.
    for (sql, expected, oids) in [
        (
            "select booleq(true, false), int4pl(1, 2), int2pl(1::int2, 2::int2), texteq('a', 'a'), \
             int4um(5), float8div(3, 2), int4div(7, 2), textcat('a', 'b')",
            "f|3|3|t|-5|1.5|3|ab",
            vec![16, 23, 21, 16, 23, 701, 23, 25],
        ),
        (
            "select 1 << 31, 1 << 33, -8 >> 33, 1::int2 << 15, 1::int2 << 16, 1::int8 << 64, \
             int4shl(1, 2)",
            "-2147483648|2|-4|-32768|0|1|4",
            vec![23, 23, 23, 21, 21, 20, 23],
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "TDCZ", "{sql}");
        let shape: Vec<u32> = row_shape(&messages[0]).into_iter().map(|(_, oid, _)| oid).collect();
        assert_eq!(shape, oids, "{sql}");
        assert_eq!(text(data_row(&messages[1])), expected, "{sql}");
    }
    let messages = client.query("select int4pl(2147483647, 1)");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("22003"));
    assert_eq!(messages[0].field(b'M').as_deref(), Some("integer out of range"));
    server.stop().unwrap();
}

#[test]
fn a_recursive_union_of_columns_that_hash_runs() {
    let dirs = Dirs::new("pgrecunion");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // PostgreSQL 19 keeps the rows of a recursive UNION in a hash table, so a column must have a
    // type whose equality hashes. These types do, and `varchar` hashes as `text`.
    for (sql, rows) in [
        (
            "with recursive t(n, s) as (values (1, 'a'::varchar) union select n + 1, s from t \
             where n < 4) select n from t",
            4,
        ),
        (
            "with recursive t(n, a) as (values (1, array['a']) union select n + 1, a || 'b'::text \
             from t where n < 3) select n from t",
            3,
        ),
        (
            "with recursive t(n) as (values (1.5::numeric) union select n from t) \
             select n from t",
            1,
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(rows)), "{sql}");
    }
    // `bit varying` compares with a btree only, so PostgreSQL refuses the query before it runs,
    // which here would never end.
    let messages = client.query(
        "with recursive t(n) as (values ('01'::varbit) union select n || '10'::varbit from t \
         where n < '100'::varbit) select n from t",
    );
    assert_eq!(tags(&messages), "EZ");
    let error = |code: u8| messages[0].field(code);
    assert_eq!(error(b'C').as_deref(), Some("0A000"));
    assert_eq!(error(b'M').as_deref(), Some("could not implement recursive UNION"));
    assert_eq!(error(b'D').as_deref(), Some("All column datatypes must be hashable."));
    // The planner refuses it, which has no place in the text to point at.
    assert_eq!(error(b'P'), None);
    server.stop().unwrap();
}

/// PostgreSQL refuses an aggregate in a block of the recursive side that reads the definition,
/// at the first aggregate, so a query that would never end gives an error. An aggregate in a
/// block that does not read it is allowed.
#[test]
fn an_aggregate_over_the_recursive_side_is_refused() {
    let dirs = Dirs::new("pgrecagg");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for (sql, at) in [
        (
            "with recursive x(n) as (select 1 union all select count(*) from x) select * from x",
            "count",
        ),
        (
            "with recursive x(n) as (select 1 union all select n + 1 from x where n < 3 \
             group by n having sum(n) > 0) select * from x",
            "sum",
        ),
        (
            "with recursive x(n) as (select 1 union all select c from (select count(*) as c \
             from x) s where c < 3) select * from x",
            "count",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        let error = |code: u8| messages[0].field(code);
        assert_eq!(error(b'C').as_deref(), Some("42P19"), "{sql}");
        assert_eq!(
            error(b'M').as_deref(),
            Some("aggregate functions are not allowed in a recursive query's recursive term")
        );
        let position = sql.find(at).map(|found| (found + 1).to_string());
        assert_eq!(error(b'P'), position, "{sql}");
    }
    let messages = client.query(
        "with recursive x(n) as (select 1 union all select n + 1 from x \
         where n < (select count(*) + 2 from (values (1)) v(a))) select * from x",
    );
    assert_eq!(tags(&messages), "TDDDCZ");
    server.stop().unwrap();
}

/// A query reads the catalog beside the queries of other sessions, so a long one keeps no other
/// query waiting, whether it comes in as a simple or as an extended query.
#[test]
fn a_long_query_keeps_no_query_of_another_session_waiting() {
    let dirs = Dirs::new("pglong");
    let server = Server::start(dirs.config()).unwrap();
    let mut long = Client::unix(&server);
    connect(&mut long, PROTOCOL_3_0);
    let mut other = Client::unix(&server);
    connect(&mut other, PROTOCOL_3_0);
    long.send(&Frontend::Query(b"select pg_sleep(3)"));
    std::thread::sleep(Duration::from_millis(300));
    let started = std::time::Instant::now();
    assert_eq!(scalar(&mut other, "select 1"), "1");
    assert_eq!(scalar(&mut other, "select count(*) from (values (1), (2)) v(x)"), "2");
    other.parse("", "select $1::int4 + 1", &[]);
    other.bind("", "", &[], &[Some(b"41")]);
    other.execute("", 0);
    assert_eq!(tags(&other.sync()), "12DCZ");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert_eq!(tags(&long.until_ready()), "TDCZ");
    server.stop().unwrap();
}

/// A temporary table, view or sequence belongs to the session that made it, and goes when that
/// session ends. One made inside a transaction that rolled back was never there.
#[test]
fn the_temporary_objects_of_a_session_go_when_it_ends() {
    let dirs = Dirs::new("tempclose");
    let server = Server::start(dirs.config()).unwrap();
    let mut first = Client::unix(&server);
    connect(&mut first, PROTOCOL_3_0);
    for sql in [
        "create temp table temptest (tcol int)",
        "create temp table counted (id serial, v int)",
        "create temp view seen as select * from temptest",
        "create temp sequence numbers",
        "begin",
        "create temp table undone (a int)",
        "rollback",
    ] {
        let messages = first.query(sql);
        assert!(messages.iter().all(|message| message.tag != b'E'), "{sql}");
    }
    first.send(&Frontend::Terminate);
    assert!(first.rest().is_empty());

    let mut second = Client::unix(&server);
    connect(&mut second, PROTOCOL_3_0);
    let left = "select count(*)::text from pg_class \
        where relname in ('temptest', 'counted', 'counted_id_seq', 'seen', 'numbers', 'undone')";
    // The session that ended can still be closing for a moment.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while scalar(&mut second, left) != "0" && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(scalar(&mut second, left), "0");
    for sql in ["create table temptest (col int)", "create index temptest_col on temptest (col)"] {
        let messages = second.query(sql);
        assert!(messages.iter().all(|message| message.tag != b'E'), "{sql}");
    }
    server.stop().unwrap();
}

/// `DISTINCT ON` keeps the first row of each key in the order of the `ORDER BY`, and gives the rows
/// in that order.
#[test]
fn distinct_on_keeps_the_first_row_in_the_order_of_order_by() {
    let dirs = Dirs::new("pgdistincton");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query(
        "create temp table kept (g text, v int, i int); insert into kept values ('A', 1, 1), ('A', 2, 2), ('B', 3, 1), ('B', 1, 2), ('B', 1, 3)",
    );
    assert!(messages.iter().all(|message| message.tag != b'E'));
    for (sql, value) in [
        (
            "select string_agg(g || v || i, ',') from (select distinct on (g) g, v, i from kept order by g, v, i) s",
            "A11,B12",
        ),
        (
            "select string_agg(g || v || i, ',') from (select distinct on (g) g, v, i from kept order by g desc, v desc, i desc) s",
            "B31,A22",
        ),
        (
            "select string_agg(g || v || i, ',') from (select distinct on (g, v) g, v, i from kept order by g, v, i desc) s",
            "A11,A22,B13,B31",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    server.stop().unwrap();
}

/// The rows whose keys tie come out of a sort and a window in the order PostgreSQL's sort leaves
/// them in: a quicksort under forty rows, and a radix sort from forty rows on when the first key is
/// an integer.
#[test]
fn rows_that_tie_are_in_the_order_postgres_sorts_them_into() {
    let dirs = Dirs::new("pgties");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query(
        "create temp table tt as select x, x % 4 as k, (x % 5)::text as s, case when x % 7 = 0 then null else x % 3 end as n from generate_series(1, 60) x",
    );
    assert!(messages.iter().all(|message| message.tag != b'E'));
    // The answers are the ones that PostgreSQL 19 gives.
    for (sql, value) in [
        (
            "select string_agg(x::text, ',') from (select x from generate_series(1, 10) x order by x % 3) s",
            "9,6,3,10,4,7,1,8,5,2",
        ),
        (
            "select string_agg(x::text, ',') from (select x from tt order by k) s",
            "4,8,12,20,24,28,40,44,56,60,48,32,36,52,16,1,5,9,13,21,25,29,41,45,57,49,37,17,33,53,2,6,10,14,22,26,30,38,42,54,58,46,34,18,50,3,7,11,15,23,27,39,43,55,59,19,51,35,31,47",
        ),
        (
            "select string_agg(x::text, ',') from (select x from tt order by n) s",
            "3,6,9,12,15,18,30,33,36,39,54,57,60,27,51,45,48,24,1,4,10,13,16,19,31,34,37,40,52,55,58,46,22,43,25,2,5,8,11,17,20,29,32,38,53,59,26,41,47,50,23,44,7,28,49,56,35,14,21,42",
        ),
        (
            "select string_agg(x::text || ':' || r, ',') from (select x, row_number() over (partition by s order by k) r from tt) s",
            "60:1,40:2,20:3,5:4,45:5,25:6,30:7,50:8,10:9,55:10,35:11,15:12,16:1,56:2,36:3,41:4,21:5,1:6,46:7,6:8,26:9,11:10,51:11,31:12,12:1,52:2,32:3,57:4,17:5,37:6,42:7,22:8,2:9,27:10,47:11,7:12,28:1,8:2,48:3,53:4,33:5,13:6,18:7,58:8,38:9,3:10,43:11,23:12,4:1,24:2,44:3,49:4,29:5,9:6,54:7,34:8,14:9,59:10,39:11,19:12",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    server.stop().unwrap();
}

/// The windows of one query are computed in the order PostgreSQL computes them in, which decides
/// the order of the rows when there is no `ORDER BY`: the windows are sorted by their keys with
/// the keys numbered in the order the clauses name them, and the last window sorts last.
#[test]
fn the_windows_of_a_query_are_computed_in_the_order_postgres_computes_them_in() {
    let dirs = Dirs::new("pgwindoworder");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query(
        "create temp table es as select * from (values ('develop',10,5200,'2007-08-01'::date),('sales',1,5000,'2006-10-01'),('personnel',5,3500,'2007-12-10'),('sales',4,4800,'2007-08-08'),('personnel',2,3900,'2006-12-23'),('develop',7,4200,'2008-01-01'),('develop',9,4500,'2008-01-01'),('sales',3,4800,'2007-08-01'),('develop',8,6000,'2006-10-01'),('develop',11,5200,'2007-08-15')) v(depname, empno, salary, enroll_date)",
    );
    assert!(messages.iter().all(|message| message.tag != b'E'));
    // The answers are the ones that PostgreSQL 19 gives.
    for (sql, value) in [
        (
            "select string_agg(empno || ':' || a || ':' || b, ',') from (select empno, rank() over (order by salary desc) a, sum(salary) over (partition by depname) b from es) s",
            "8:1:25100,10:2:25100,11:2:25100,1:4:14600,4:5:14600,3:5:14600,9:7:25100,7:8:25100,2:9:7400,5:10:7400",
        ),
        (
            "select string_agg(empno || ':' || a || ':' || b, ',') from (select empno, sum(salary) over (partition by depname) a, rank() over (order by salary desc) b from es) s",
            "9:25100:7,10:25100:2,11:25100:2,8:25100:1,7:25100:8,5:7400:10,2:7400:9,4:14600:5,1:14600:4,3:14600:5",
        ),
        (
            "select string_agg(empno || ':' || a || ':' || b, ',') from (select empno, sum(salary) over (partition by depname order by salary) a, count(*) over (order by salary) b from es) s",
            "5:3500:1,2:7400:2,7:4200:3,9:8700:4,3:9600:6,4:9600:6,1:14600:7,11:19100:9,10:19100:9,8:25100:10",
        ),
        (
            "select string_agg(empno || ':' || a || ':' || b, ',') from (select empno, row_number() over (partition by depname order by enroll_date) a, row_number() over (partition by depname order by enroll_date desc) b from es) s",
            "8:1:5,10:2:4,11:3:3,9:4:2,7:5:1,2:1:2,5:2:1,1:1:3,3:2:2,4:3:1",
        ),
        (
            "select string_agg(empno || ':' || a || ':' || b, ',') from (select empno, count(*) over (partition by enroll_date) a, sum(salary) over w b from es window w as (partition by depname)) s",
            "8:2:25100,9:2:25100,7:2:25100,10:2:25100,11:1:25100,5:1:7400,2:1:7400,4:1:14600,3:2:14600,1:2:14600",
        ),
        (
            "select string_agg(empno || ':' || a || ':' || b, ',') from (select empno, sum(salary) over (partition by depname) a, count(*) over (partition by enroll_date) b from es order by salary) s",
            "5:7400:1,2:7400:1,7:25100:2,9:25100:2,3:14600:2,4:14600:1,1:14600:2,11:25100:1,10:25100:2,8:25100:2",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    server.stop().unwrap();
}

/// `extract` gives a numeric with the value and the scale that PostgreSQL gives and `date_part` a
/// double, both read a time stamp with a time zone in the zone of the session, and both refuse a
/// unit as PostgreSQL does. A function with a body in SQL, such as `date_part(text, date)` or
/// `round(numeric)`, is its body.
#[test]
fn extract_and_date_part_give_the_answers_that_postgres_gives() {
    let dirs = Dirs::new("pgextract");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The answers are the ones that PostgreSQL 19 gives.
    for (sql, value) in [
        (
            "select extract(epoch from timestamptz '2020-07-01 12:00:00.123456+00')::text",
            "1593604800.123456",
        ),
        (
            "select extract(julian from timestamp '2020-07-01 18:00:00')::text",
            "2459032.75000000000000000000",
        ),
        ("select extract(j from date '2020-07-01')::text", "2459032"),
        (
            "select extract(epoch from interval '1 year 2 months 3 days 4.5 seconds')::text",
            "37000804.500000",
        ),
        ("select extract(millisecond from interval '1.234567 s')::text", "1234.567"),
        ("select extract(epoch from timetz '01:02:03.5+05')::text", "-14276.500000"),
        ("select extract(year from timestamp 'infinity')::text", "Infinity"),
        ("select coalesce(extract(month from timestamp '-infinity')::text, 'null')", "null"),
        ("select pg_typeof(extract(year from now()))::text", "numeric"),
        ("select extract(century from date '0001-01-01 BC')::text", "-1"),
        ("select extract(\"Year\" from date '2020-07-01')::text", "2020"),
        ("select pg_typeof(date_part('year', date '2020-07-01'))::text", "double precision"),
        ("select date_part('year', date 'infinity')::text", "Infinity"),
        ("select date_part('hour', date '2020-07-01')::text", "0"),
        (
            "select date_part('julian', timestamp '2020-07-01 18:00:01.5')::text",
            "2459032.7500173612",
        ),
        (
            "select date_part('epoch', interval '1 year 2 months 3 days 4.5 seconds')::text",
            "37000804.5",
        ),
        ("select date_part('second', timetz '01:02:03.123+05')::text", "3.123"),
        ("select date_part('millisecond', time '00:00:56.789012')::text", "56789.012"),
        ("select round(2.5)::text", "3"),
        ("select log(100.0)::text", "2.0000000000000000"),
        ("select lpad('ab', 5)", "   ab"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    assert_eq!(tags(&client.query("set time zone 'America/New_York'")), "CSZ");
    assert_eq!(
        scalar(
            &mut client,
            "select extract(timezone from timestamptz '2020-07-01 12:00+00')::text"
        ),
        "-14400"
    );
    assert_eq!(
        scalar(&mut client, "select date_part('hour', timestamptz '2020-07-01 12:00+00')::text"),
        "8"
    );
    for (sql, state, message) in [
        (
            "select extract(hour from date '2020-07-01')",
            "0A000",
            "unit \"hour\" not supported for type date",
        ),
        (
            "select extract(foo from timestamp '2020-07-01')",
            "22023",
            "unit \"foo\" not recognized for type timestamp without time zone",
        ),
        (
            "select date_part('day', time '01:00')",
            "0A000",
            "unit \"day\" not supported for type time without time zone",
        ),
        (
            "select date_part('now', interval '1 day')",
            "22023",
            "unit \"now\" not recognized for type interval",
        ),
        (
            "select pg_catalog.extract('year', '2020-07-01')",
            "42725",
            "function pg_catalog.extract(unknown, unknown) is not unique",
        ),
        (
            "select pg_catalog.extract('year', 1)",
            "42883",
            "function pg_catalog.extract(unknown, integer) does not exist",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

/// The name, the type and the type modifier of each column of a row description.
fn typed_shape(message: &Message) -> Vec<(String, u32, i32)> {
    let bytes = message.decoded();
    let Backend::RowDescription(fields) = Backend::decode(&bytes).unwrap().unwrap().0 else {
        panic!("{message:?}");
    };
    let shape = fields
        .iter()
        .map(|f| (String::from_utf8_lossy(f.name).into_owned(), f.type_oid, f.type_modifier));
    shape.collect()
}

/// A `sum` over a window has the type it has over a group, a column of a set operation has the
/// declared type and modifier of its two sides, a subscripted subquery is named for its column, and
/// a scalar subquery of two rows is error 21000, all as in PostgreSQL.
#[test]
fn windows_set_operations_and_subqueries_have_the_types_and_names_of_postgres() {
    let dirs = Dirs::new("pgshapes");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query(
        "create temp table shaped (s int4, m int2, f float4, c char(4), v varchar(4)); insert into shaped values (1, 2, 1.5, 'a', 'ab'), (3, 4, 2.5, 'bc', 'c')",
    );
    assert!(messages.iter().all(|message| message.tag != b'E'));
    let shape = |client: &mut Client, sql: &str| {
        let messages = client.query(sql);
        let description = messages.iter().find(|message| message.tag == b'T');
        typed_shape(description.unwrap_or_else(|| panic!("{sql}: {messages:?}")))
    };
    let column = |name: &str, oid: u32, typmod: i32| (name.to_owned(), oid, typmod);
    assert_eq!(
        shape(
            &mut client,
            "select sum(s) over (), sum(m) over (order by s), sum(f) over () from shaped"
        ),
        [column("sum", 20, -1), column("sum", 20, -1), column("sum", 700, -1)]
    );
    assert_eq!(
        scalar(
            &mut client,
            "select string_agg(t::text, ',') from (select sum(s) over (order by s) t from shaped) w"
        ),
        "1,4"
    );
    for (sql, oid, typmod) in [
        ("select cast(v as char(4)) as x from shaped union select c from shaped", 1042, 8),
        ("select c as x from shaped union all select v from shaped", 1042, -1),
        ("select v as x from shaped union all select v from shaped", 1043, 8),
        ("select c as x from shaped union all select 'z'", 1042, -1),
        (
            "select c as x from shaped union select c from shaped union select c from shaped",
            1042,
            8,
        ),
        ("select s as x from shaped union select m from shaped", 23, -1),
    ] {
        assert_eq!(shape(&mut client, sql), [column("x", oid, typmod)], "{sql}");
    }
    let rows = client.query("select c from shaped union select c from shaped order by 1");
    let values: Vec<_> = rows.iter().filter(|message| message.tag == b'D').map(data_row).collect();
    assert_eq!(values, [[Some(b"a   ".to_vec())], [Some(b"bc  ".to_vec())]]);
    assert_eq!(
        shape(&mut client, "select (select array[1, 2, 3])[1], (select case when true then 1 end)"),
        [column("array", 23, -1), column("case", 23, -1)]
    );
    // The error comes as the query runs, after the row description.
    let messages = client.query("select (select 1 union all select 2)");
    assert_eq!(tags(&messages), "TEZ");
    assert_eq!(messages[1].field(b'C').as_deref(), Some("21000"));
    assert_eq!(
        messages[1].field(b'M').as_deref(),
        Some("more than one row returned by a subquery used as an expression")
    );
    server.stop().unwrap();
}

/// `VACUUM` and `ANALYZE` check their options, tables and columns as PostgreSQL does, warn for a
/// view they skip, and a `VACUUM` does not run in a transaction block.
#[test]
fn vacuum_and_analyze_check_their_options_tables_and_columns() {
    let dirs = Dirs::new("pgvacuum");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query(
        "create temp table kept (a int, b text); create temp view kept_v as select a from kept",
    );
    assert!(messages.iter().all(|message| message.tag != b'E'));
    for (sql, tag) in [
        ("analyze kept", "ANALYZE"),
        ("analyse kept(b)", "ANALYZE"),
        ("vacuum analyze kept(a)", "VACUUM"),
        ("vacuum (freeze, index_cleanup auto, parallel 2) kept", "VACUUM"),
        ("analyze (buffer_usage_limit '256kB') kept", "ANALYZE"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "CZ", "{sql}");
        assert_eq!(text(&messages[0]), tag, "{sql}");
    }
    for (sql, code, message, position) in [
        ("vacuum (bogus) kept", "42601", "unrecognized VACUUM option \"bogus\"", Some("9")),
        ("analyze (analyze) kept", "42601", "unrecognized ANALYZE option \"analyze\"", Some("10")),
        (
            "vacuum (parallel 2000) kept",
            "42601",
            "PARALLEL option must be between 0 and 1024",
            Some("9"),
        ),
        (
            "vacuum kept(a)",
            "0A000",
            "ANALYZE option must be specified when a column list is provided",
            None,
        ),
        ("analyze kept(z)", "42703", "column \"z\" of relation \"kept\" does not exist", None),
        (
            "analyze kept(a, A)",
            "42701",
            "column \"a\" of relation \"kept\" appears more than once",
            None,
        ),
        ("analyze nope", "42P01", "relation \"nope\" does not exist", None),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), position, "{sql}");
    }
    for (sql, action) in [("vacuum kept_v", "vacuum"), ("analyze kept_v", "analyze")] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "NCZ", "{sql}");
        assert_eq!(messages[0].field(b'S').as_deref(), Some("WARNING"));
        assert_eq!(messages[0].field(b'C').as_deref(), Some("01000"));
        let skipping =
            format!("skipping \"kept_v\" --- cannot {action} non-tables or special system tables");
        assert_eq!(messages[0].field(b'M'), Some(skipping));
    }
    client.query("begin");
    assert_eq!(tags(&client.query("analyze kept")), "CZ");
    let messages = client.query("vacuum kept");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("25001"));
    assert_eq!(
        messages[0].field(b'M').as_deref(),
        Some("VACUUM cannot run inside a transaction block")
    );
    client.query("rollback");
    server.stop().unwrap();
}

/// `ORDER BY ... USING op` sorts as the btree family that has the operator as its `<` or its `>`,
/// in a query, in a window and in an aggregate, and another operator is the error of PostgreSQL.
#[test]
fn an_order_by_using_sorts_as_the_operator_family() {
    let dirs = Dirs::new("pgusing");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client
        .query("create temp table sorted (a int, b text); insert into sorted values (1, 'x'), (3, 'y'), (null, 'z'), (2, null)");
    assert!(messages.iter().all(|message| message.tag != b'E'));
    for (sql, value) in [
        (
            "select string_agg(coalesce(a::text, 'n'), ',') from (select a from sorted order by a using >) s",
            "n,3,2,1",
        ),
        (
            "select string_agg(coalesce(a::text, 'n'), ',') from (select a from sorted order by a using <) s",
            "1,2,3,n",
        ),
        ("select string_agg(b, ',' order by b using ~>~) from sorted", "z,y,x"),
        (
            "select string_agg(a::text, ',' order by a using operator(pg_catalog.>)) from sorted",
            "3,2,1",
        ),
        (
            "select string_agg(r::text, ',') from (select row_number() over (order by a using > nulls last) as r from sorted order by a) s",
            "3,2,1,4",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    for (sql, state, message, position) in [
        (
            "select a from sorted order by a using =",
            "42809",
            "operator = is not a valid ordering operator",
            "39",
        ),
        (
            "select a from sorted order by a using @@",
            "42883",
            "operator does not exist: integer @@ integer",
            "39",
        ),
    ] {
        let messages = client.query(sql);
        let error = messages.iter().find(|message| message.tag == b'E').unwrap();
        assert_eq!(error.field(b'C').unwrap(), state, "{sql}");
        assert_eq!(error.field(b'M').unwrap(), message);
        assert_eq!(error.field(b'P').unwrap(), position, "{sql}");
    }
    server.stop().unwrap();
}

/// A string compared with a `name` column is a `name`, so the input of `name` cuts it to 63 bytes
/// before the comparison, as in PostgreSQL.
#[test]
fn a_string_compared_with_a_name_is_a_name() {
    let dirs = Dirs::new("pgnamecmp");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let long = "1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890ABCDEFGHIJKLMNOPQR";
    let messages = client.query(&format!(
        "create temp table names (f1 name); insert into names values ('{long}'), ('abc')"
    ));
    assert!(messages.iter().all(|message| message.tag != b'E'));
    for (test, count) in [
        (format!("f1 = '{long}'"), "1"),
        (format!("'{long}' = f1"), "1"),
        (format!("f1 <> '{long}'"), "1"),
        (format!("f1 < '{long}'"), "0"),
        ("f1 ~ '^123'".to_string(), "1"),
    ] {
        let sql = format!("select count(*)::text from names where {test}");
        assert_eq!(scalar(&mut client, &sql), count, "{test}");
    }
    server.stop().unwrap();
}

/// A prefix operator of `pg_operator` that the grammar does not name, such as `@` for the absolute
/// value, is the operator PostgreSQL finds for the type of its operand.
#[test]
fn a_prefix_operator_is_the_one_postgres_finds() {
    let dirs = Dirs::new("pgprefix");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for (sql, value) in [
        ("select (@ -5)::text", "5"),
        ("select pg_typeof(@ -5::int2)::text", "smallint"),
        ("select (@ '-5.5')::text", "5.5"),
        ("select pg_typeof(@ '-5')::text", "double precision"),
        ("select (|/ 16)::text", "4"),
        ("select pg_typeof(|/ 16::int2)::text", "double precision"),
        ("select (||/ 27.0)::text", "3"),
        ("select coalesce((@ null)::text, 'null')", "null"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    for (sql, message) in [
        ("select @ 'abc'::text", "operator does not exist: @ text"),
        ("select @-@ 1", "operator does not exist: @-@ integer"),
    ] {
        let messages = client.query(sql);
        let error = messages.iter().find(|message| message.tag == b'E').unwrap();
        assert_eq!(error.field(b'C').unwrap(), "42883", "{sql}");
        assert_eq!(error.field(b'M').unwrap(), message);
        assert_eq!(error.field(b'P').unwrap(), "8", "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn an_operator_between_numbers_is_the_operator_postgres_finds() {
    let dirs = Dirs::new("pgnumops");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // PostgreSQL prefers `float8` in the numeric category, so a `real` with another number is a
    // `double precision`. The values are those of PostgreSQL 19.
    for (sql, value) in [
        ("select pg_typeof(1::float4 + 1.0)::text", "double precision"),
        ("select pg_typeof(1::float4 * 1::int4)::text", "double precision"),
        ("select pg_typeof(1::float4 - 1::int8)::text", "double precision"),
        ("select pg_typeof(1::numeric + 1::float4)::text", "double precision"),
        ("select pg_typeof(1::float4 + 1::float4)::text", "real"),
        ("select pg_typeof(1::float4 + '1')::text", "real"),
        ("select pg_typeof(1::int2 + 1.5)::text", "numeric"),
        ("select pg_typeof(1::int8 + 1::int2)::text", "bigint"),
        ("select (0.1::float4 + 0.2)::text", "0.30000000149011613"),
        ("select (0.1::float4 = 0.1)::text", "false"),
        ("select ('Infinity'::float4 + 100.0)::text", "Infinity"),
        ("select (10 / 4.0)::text", "2.5000000000000000"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    let messages = client.query("select 1::float4 % 1");
    let error = messages.iter().find(|message| message.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').unwrap(), "42883");
    assert_eq!(error.field(b'M').unwrap(), "operator does not exist: real % integer");
    assert_eq!(error.field(b'P').unwrap(), "18");
    server.stop().unwrap();
}

#[test]
fn a_key_of_a_type_with_no_equality_or_ordering_is_an_error() {
    let dirs = Dirs::new("pgsortops");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // `json` has no default btree or hash operator class, so it has no equality operator and no
    // ordering operator. The texts and the positions are those of PostgreSQL 19.
    let equality = "could not identify an equality operator for type json";
    let ordering = "could not identify an ordering operator for type json";
    for (sql, message, position) in [
        ("select distinct '{}'::json", equality, "17"),
        ("select 1, '{}'::json union select 1, '{}'::json", equality, "11"),
        ("select '{}'::json group by 1", equality, "28"),
        ("select count(distinct '{}'::json)", equality, "23"),
        ("select 1 from (values ('{}'::json)) v(j) order by j", ordering, "51"),
        ("select distinct on (j) j from (values ('{}'::json)) v(j)", equality, "21"),
        (
            "select distinct array['{}'::json]",
            "could not identify an equality operator for type json[]",
            "17",
        ),
        (
            "select 1 from (values ('{}'::json)) v(j) order by array[j]",
            "could not identify an ordering operator for type json[]",
            "51",
        ),
        ("select array_agg(j order by j) from (values ('{}'::json)) v(j)", ordering, "29"),
        (
            "select row_number() over (partition by j) from (values ('{}'::json)) v(j)",
            equality,
            "40",
        ),
        ("select row_number() over (order by j) from (values ('{}'::json)) v(j)", ordering, "36"),
        (
            "with recursive r(j) as (select '{}'::json union select j from r) select 1 from r",
            equality,
            "32",
        ),
        (
            "select j from (values ('{}'::json)) v(j) union all select '{}'::json order by 1",
            ordering,
            "79",
        ),
        ("select '{}'::json union select '{}'::json union select '{}'::json", equality, "8"),
        ("select * from (select 1, '{}'::json union select 2, '{}'::json) s", equality, "26"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42883"), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(position), "{sql}");
        let hint = message
            .contains("ordering")
            .then_some("Use an explicit ordering operator or modify the query.");
        assert_eq!(messages[0].field(b'H').as_deref(), hint, "{sql}");
    }
    // These types have both operators, `varchar` through the class of `text`, and `xid` has an
    // equality from its hash class.
    for (sql, rows) in [
        ("select distinct '{}'::jsonb, 'a'::varchar, array[1], 1.5::numeric, ''::bytea", 1),
        ("select '{}'::jsonb union select '{}'::jsonb", 1),
        ("select count(distinct x) from (values ('a'::varchar), ('a')) v(x)", 1),
        ("select x from (values (array['a']), (array['a'])) v(x) group by x order by x", 1),
        ("select row_number() over (partition by '{}'::json::text order by 1) ", 1),
        ("select '{}'::json union all select '{}'::json", 2),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(rows)), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_string_functions_of_postgres_give_its_values_and_its_errors() {
    let dirs = Dirs::new("pgstring");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // psql writes a null as an empty string, and so does this.
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The values are the ones that PostgreSQL 19 gives in a database whose collation is `C`, which
    // maps the case of only the ASCII letters.
    for (sql, expected) in [
        (
            "select btrim('  a  '), btrim('xxaxx', 'x'), ltrim('  a'), rtrim('a  '), \
            ltrim('xxa', 'x'), rtrim('axx', 'x');",
            "a|a|a|a|a|a",
        ),
        (
            "select bit_length('ab'), char_length('abc'), character_length('é'), \
            octet_length('é'), length('abc');",
            "16|3|1|2|3",
        ),
        (
            "select lower('ABÉ'), upper('abé'), initcap('hello wORLD foo_bar 1abc');",
            "abÉ|ABé|Hello World Foo_Bar 1abc",
        ),
        (
            "select lpad('abc', 5), lpad('abc', 5, 'xy'), lpad('abc', 2), rpad('abc', 5, 'xy'), \
            lpad('abc', 5, ''), lpad('a', -1);",
            "  abc|xyabc|ab|abcxy|abc|",
        ),
        (
            "select overlay('abcdef' placing 'XY' from 2), overlay('abcdef' placing 'XY' from 2 \
            for 0);",
            "aXYdef|aXYbcdef",
        ),
        ("select position('c' in 'abc'), strpos('abc', ''), position('' in 'abc');", "3|1|1"),
        (
            "select trim(leading 'x' from 'xxaxx'), trim(trailing from '  a  '), trim(both from \
            '  a  '), trim('  a  ');",
            "axx|  a|a|a",
        ),
        ("select ascii('a'), ascii('é'), ascii(''), chr(65), chr(233);", "97|233|0|A|é"),
        (
            "select concat('a', null, 1), concat_ws(',', 'a', null, 'b'), concat_ws(null, 'a');",
            "a1|a,b|",
        ),
        (
            "select format('%s %s', 'a', 1), format('%I', 'a b'), format('%L', 'it''s'), \
            format('%L', null), format('%s', null);",
            "a 1|\"a b\"|'it''s'|NULL|",
        ),
        (
            "select format('%2$s %1$s', 'a', 'b'), format('%-5s|', 'a'), format('%5s|', 'a'), \
            format('%*s|', 4, 'a'), format('%%');",
            "b a|a    ||    a||   a||%",
        ),
        (
            "select format(null), format('x', null), format('%s %s', variadic array['a', 'b']);",
            "|x|a b",
        ),
        (
            "select left('abc', 2), left('abc', -1), right('abc', 2), right('abc', -1), \
            left('abc', 0);",
            "ab|ab|bc|bc|",
        ),
        (
            "select md5('abc'), md5(''::bytea);",
            "900150983cd24fb0d6963f7d28e17f72|d41d8cd98f00b204e9800998ecf8427e",
        ),
        ("select parse_ident('a.b'), parse_ident('\"A b\".c');", "{a,b}|{\"A b\",c}"),
        (
            "select quote_ident('a'), quote_ident('a b'), quote_ident('A'), \
            quote_ident('select'), quote_ident('a\"b');",
            "a|\"a b\"|\"A\"|\"select\"|\"a\"\"b\"",
        ),
        (
            "select quote_literal('a'), quote_literal('it''s'), quote_literal(e'a\\\\b'), \
            quote_literal(1), quote_nullable(null), quote_nullable('a');",
            "'a'|'it''s'|E'a\\\\b'|'1'|NULL|'a'",
        ),
        (
            "select repeat('ab', 3), repeat('ab', 0), repeat('ab', -1), replace('abcabc', 'b', \
            'X'), replace('abc', '', 'X');",
            "ababab|||aXcaXc|abc",
        ),
        ("select reverse('abc'), reverse('é');", "cba|é"),
        (
            "select split_part('a,b,c', ',', 2), split_part('a,b,c', ',', -1), \
            split_part('a,b,c', ',', 5), split_part('a,b,c', '', 1);",
            "b|c||a,b,c",
        ),
        ("select starts_with('abc', 'ab'), starts_with('abc', '');", "t|t"),
        (
            "select string_to_array('a,b,,c', ','), string_to_array('a,b,,c', ',', ''), \
            string_to_array('abc', null), string_to_array('abc', ''), string_to_array('', ',');",
            "{a,b,\"\",c}|{a,b,NULL,c}|{a,b,c}|{abc}|{}",
        ),
        (
            "select substr('abc', 2), substr('abc', 2, 1), substring('abc', 2), substring('abc' \
            for 2);",
            "bc|b|bc|ab",
        ),
        ("select translate('abc', 'ab', 'x'), translate('abc', '', 'x');", "xc|abc"),
        ("select unistr('d\\0061t\\+000061'), unistr('é');", "data|é"),
        ("select casefold('ABC');", "abc"),
        (
            "select bit_count('\\x0f'::bytea), get_bit('\\x0f'::bytea, 0), \
            get_byte('\\x0f'::bytea, 0), set_bit('\\x00'::bytea, 0, 1), \
            set_byte('\\x00'::bytea, 0, 65);",
            "4|1|15|\\x01|\\x41",
        ),
        (
            "select length('\\x0102'::bytea), octet_length('\\x0102'::bytea), \
            btrim('\\x000100'::bytea, '\\x00'::bytea), substr('\\x010203'::bytea, 2, 1);",
            "2|2|\\x01|\\x02",
        ),
        (
            "select sha224('abc'), sha256('abc'), sha384(''), sha512('');",
            "\\x23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7|\\xba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad|\\x38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b|\\xcf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
        ),
        (
            "select encode('abc'::bytea, 'hex'), encode('abc'::bytea, 'base64'), \
            encode('a\\000b'::bytea, 'escape'), encode('abc', 'base64url');",
            "616263|YWJj|a\\000b|YWJj",
        ),
        (
            "select decode('616263', 'hex'), decode('YWJj', 'base64'), decode('a\\000b', \
            'escape');",
            "\\x616263|\\x616263|\\x610062",
        ),
        (
            "select convert_from('abc'::bytea, 'UTF8'), convert_to('abc', 'UTF8'), \
            convert('abc'::bytea, 'UTF8', 'LATIN1');",
            "abc|\\x616263|\\x616263",
        ),
        ("select crc32('abc'::bytea), crc32c('abc'::bytea);", "891568578|910901175"),
        ("select pg_client_encoding(), to_hex(-1), to_bin(5), to_oct(8);", "UTF8|ffffffff|101|10"),
        ("select 'abc' || 1, 1 || 'abc', 'a' || null, null::text || 'a';", "abc1|1abc||"),
        ("select 'abc' like 'a%', 'abc' ilike 'A%', 'abc' ~~ 'a_c', 'abc' !~~ 'x%';", "t|t|t|t"),
        (
            "select ltrim('\\x000100'::bytea, '\\x00'::bytea), rtrim('\\x000100'::bytea, \
            '\\x00'::bytea);",
            "\\x0100|\\x0001",
        ),
        ("select reverse('\\x0102'::bytea);", "\\x0201"),
        (
            "select parse_ident('a b', false), parse_ident('A.b c', false), \
            parse_ident('a.\"b\"c', false);",
            "{a}|{a,b}|{a,b}",
        ),
        ("select parse_ident('a.b.c.d'), parse_ident(' a . \"B\" ');", "{a,b,c,d}|{a,B}"),
        (
            "select quote_ident(''), quote_ident('_a1$'), quote_ident('a$'), quote_ident('1a'), \
            quote_ident('é'), quote_ident('int'), quote_ident('abort'), quote_ident('between'), \
            quote_ident('user');",
            "\"\"|\"_a1$\"|\"a$\"|\"1a\"|\"é\"|\"int\"|abort|\"between\"|\"user\"",
        ),
        (
            "select format('%s', array[1,2]), format('%L', 1.5), format('%L', true), \
            format('%I', 1), format('%s', 1.5::float8), format('%L', array['a']);",
            "{1,2}|'1.5'|'t'|\"1\"|1.5|'{a}'",
        ),
        ("select unistr('\\d83d\\de00');", "😀"),
        ("select unistr('\\110000');", "ᄀ00"),
        (
            "select split_part('abc', 'abc', 1), split_part('abc', 'abc', 2), split_part('', \
            ',', 1), split_part('a,b', ',', -3);",
            "|||",
        ),
        (
            "select string_to_array('a,b', ',', 'b'), string_to_array(null, ','), \
            string_to_array('a,,b', null, 'a'), string_to_array('abc', null, 'b');",
            "{a,NULL}||{NULL,\",\",\",\",b}|{a,NULL,c}",
        ),
        (
            "select string_to_array('', ''), string_to_array('', null), string_to_array('ab', \
            'ab', 'ab');",
            "{}|{}|{\"\",\"\"}",
        ),
        (
            "select set_bit('\\x0000'::bytea, 9, 1), set_byte('\\x00'::bytea, 0, 256), \
            set_byte('\\x00'::bytea, 0, -1);",
            "\\x0002|\\x00|\\xff",
        ),
        ("select decode('YW=j', 'base64');", "\\x61"),
        ("select decode('\\\\\\101', 'escape'), decode('61 62', 'hex');", "\\x5c41|\\x6162"),
        ("select encode('', 'hex'), decode('', 'base64'), encode('a', 'HEX');", "|\\x|61"),
        (
            "select convert_to('é', 'LATIN1'), convert_from('\\xe9', 'LATIN1'), \
            convert('\\xc3a9', 'UTF8', 'LATIN1'), convert_to('é', 'SQL_ASCII');",
            "\\xe9|é|\\xe9|\\xc3a9",
        ),
        ("select chr(127), chr(128), length(chr(1114111));", "||1"),
        (
            "select initcap('ÉCOLE éCOLE'), initcap('a-b c''d'), initcap(''), initcap('ǆa');",
            "ÉCole éCole|A-B C'D||ǆA",
        ),
        (
            "select initcap('ÉCOLE éCOLE' collate pg_c_utf8), initcap('ǆa' collate pg_c_utf8);",
            "École École|Ǆa",
        ),
        ("select casefold('ẞ ß Σ'), lower('ẞ ß Σ');", "ẞ ß Σ|ẞ ß Σ"),
        (
            "select casefold('ẞ ß Σ' collate pg_c_utf8), lower('ẞ ß Σ' collate pg_c_utf8);",
            "ß ß σ|ß ß σ",
        ),
        (
            "select bit_count('\\xff00'::bytea), crc32(''), crc32c(''), sha224('');",
            "8|0|0|\\xd14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f",
        ),
        (
            "select btrim('xyaxy', 'yx'), btrim('', 'x'), btrim('a', ''), ltrim('ééa', 'é'), \
            btrim('\\x01'::bytea, ''::bytea);",
            "a||a|a|\\x01",
        ),
        ("select to_ascii('abc', 'LATIN1');", "abc"),
        ("select to_ascii('abc', 8);", "abc"),
        ("select repeat('', 1000000000) = '';", "t"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "TDCZ", "{sql}");
        assert_eq!(text(data_row(&messages[1])), expected, "{sql}");
    }
    for (sql, state, message) in [
        ("select chr(0);", "54000", "null character not permitted"),
        ("select chr(-1);", "22023", "character number must be positive"),
        ("select chr(1114112);", "54000", "requested character too large for encoding: 1114112"),
        ("select format('%s');", "22023", "too few arguments for format()"),
        ("select format('%z', 1);", "22023", "unrecognized format() type specifier \"z\""),
        ("select split_part('a,b', ',', 0);", "22023", "field position must not be zero"),
        (
            "select to_ascii('abc');",
            "0A000",
            "encoding conversion from UTF8 to ASCII not supported",
        ),
        ("select unistr('\\xyz');", "42601", "invalid Unicode escape"),
        ("select decode('6', 'hex');", "22023", "invalid hexadecimal data: odd number of digits"),
        ("select encode('abc'::bytea, 'nope');", "22023", "unrecognized encoding: \"nope\""),
        ("select repeat('abc', 500000000);", "54000", "requested length too large"),
        (
            "select parse_ident('a.'), parse_ident('\"a'), parse_ident('a b'), parse_ident('.a');",
            "22023",
            "string is not a valid identifier: \"a.\"",
        ),
        ("select parse_ident('a.');", "22023", "string is not a valid identifier: \"a.\""),
        ("select parse_ident('\"a');", "22023", "string is not a valid identifier: \"\"a\""),
        ("select parse_ident('a b');", "22023", "string is not a valid identifier: \"a b\""),
        ("select parse_ident('\"\"');", "22023", "string is not a valid identifier: \"\"\"\""),
        ("select parse_ident('');", "22023", "string is not a valid identifier: \"\""),
        (
            "select format('%I', null);",
            "22004",
            "null values cannot be formatted as an SQL identifier",
        ),
        (
            "select format('%1$s %s', 'a', 'b'), format('%3$s', 'a');",
            "22023",
            "too few arguments for format()",
        ),
        (
            "select format('%0$s', 'a');",
            "22023",
            "format specifies argument 0, but arguments are numbered from 1",
        ),
        (
            "select format('%-*s|', -4, 'a'), format('%*s|', -4, 'a'), format('%*2$s|', 'a', 5), \
            format('%1$*2$s|', 'a', 5);",
            "22023",
            "too few arguments for format()",
        ),
        (
            "select format('%*s|', null, 'a'), format('%*s|', 'x', 'a');",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
        ),
        ("select format('%', 'a');", "22023", "unterminated format() type specifier"),
        ("select format('%1', 'a');", "22023", "unterminated format() type specifier"),
        ("select format('%s %', 'a');", "22023", "unterminated format() type specifier"),
        (
            "select format('%s', variadic null::text[]), format('%s %s', variadic array[1, 2]);",
            "22023",
            "too few arguments for format()",
        ),
        (
            "select format('%s', variadic null::text[]) is null;",
            "22023",
            "too few arguments for format()",
        ),
        (
            "select unistr('é'), unistr('\\U0001F600'), unistr('a\\\\b'), unistr('\\+00E9'), \
            unistr('\\00e9');",
            "42601",
            "invalid Unicode escape",
        ),
        ("select unistr('\\u12');", "42601", "invalid Unicode escape"),
        ("select unistr('\\d800');", "42601", "invalid Unicode surrogate pair"),
        ("select unistr('a\\');", "42601", "invalid Unicode escape"),
        ("select get_bit('\\x0f'::bytea, 16);", "2202E", "index 16 out of valid range, 0..7"),
        ("select get_byte('\\x0f'::bytea, -1);", "2202E", "index -1 out of valid range, 0..0"),
        ("select set_byte('\\x0f'::bytea, 1, 0);", "2202E", "index 1 out of valid range, 0..0"),
        ("select set_bit('\\x00'::bytea, 0, 2);", "22023", "new bit must be 0 or 1"),
        (
            "select get_bit('\\x80'::bytea, 7), get_bit('\\x01'::bytea, 0), \
            set_bit('\\x00'::bytea, 9, 1);",
            "2202E",
            "index 9 out of valid range, 0..7",
        ),
        (
            "select decode('YW Jj', 'base64'), decode('YWI=', 'base64'), decode('YWI', \
            'base64url'), decode('6g', 'hex');",
            "22023",
            "invalid hexadecimal digit: \"g\"",
        ),
        ("select decode('Y', 'base64');", "22023", "invalid base64 end sequence"),
        ("select decode('a\\', 'escape');", "22P02", "invalid input syntax for type bytea"),
        ("select decode('a\\9', 'escape');", "22P02", "invalid input syntax for type bytea"),
        ("select decode('6', 'hex');", "22023", "invalid hexadecimal data: odd number of digits"),
        ("select decode('x', 'nope');", "22023", "unrecognized encoding: \"nope\""),
        (
            "select convert_to('€', 'LATIN1');",
            "22P05",
            "character with byte sequence 0xe2 0x82 0xac in encoding \"UTF8\" has no equivalent \
            in encoding \"LATIN1\"",
        ),
        (
            "select convert_from('\\xff', 'UTF8');",
            "22021",
            "invalid byte sequence for encoding \"UTF8\": 0xff",
        ),
        ("select convert_to('a', 'nope');", "22023", "invalid destination encoding name \"nope\""),
        ("select chr(55296);", "54000", "requested character not valid for encoding: 55296"),
    ] {
        // An error of a kernel comes after the row description.
        let messages = client.query(sql);
        assert!(["EZ", "TEZ"].contains(&tags(&messages).as_str()), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn string_to_table_and_normalize_give_the_rows_and_the_values_of_postgres() {
    let dirs = Dirs::new("pgrows");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The rows are the ones that PostgreSQL 19 gives, with a null as an empty string.
    let cases: [(&str, &[&str]); 27] = [
        ("select * from string_to_table('a,b', ',');", &["a", "b"]),
        ("select string_to_table('a,b,,c', ',', '');", &["a", "b", "", "c"]),
        (
            "select * from string_to_table('a|b|x', '|', 'x') with ordinality;",
            &["a|1", "b|2", "|3"],
        ),
        ("select string_to_table('abc', null);", &["a", "b", "c"]),
        ("select string_to_table('abc', '');", &["abc"]),
        ("select string_to_table(null, ',');", &[]),
        ("select string_to_table('', ',');", &[]),
        ("select pg_typeof(string_to_table('a', ','));", &["text"]),
        ("select * from regexp_split_to_table('a,b', ',');", &["a", "b"]),
        ("select regexp_split_to_table('a1b2c', '\\d');", &["a", "b", "c"]),
        ("select normalize('a');", &["a"]),
        ("select normalize(U&'\\0061\\0301', nfc) = U&'\\00E1';", &["t"]),
        (
            "select normalize(U&'\\00E1', NFD) = U&'\\0061\\0301', normalize(U&'\\FB01', nfkc), normalize(U&'\\FB01', nfkd);",
            &["t|fi|fi"],
        ),
        (
            "select U&'\\00E1' is normalized, U&'\\0061\\0301' is nfc normalized, U&'\\0061\\0301' is not nfd normalized, U&'\\FB01' is nfkc normalized;",
            &["t|f|f|f"],
        ),
        ("select is_normalized('a', 'NFC'), is_normalized(U&'\\0061\\0301');", &["t|f"]),
        ("select normalize(null), null::text is normalized;", &["|"]),
        ("select string_to_table('a,b,c', ','), generate_series(1, 2);", &["a|1", "b|2", "c|"]),
        (
            "select x, string_to_table(x, '-') from (values ('p-q'), ('r')) t(x);",
            &["p-q|p", "p-q|q", "r|r"],
        ),
        (
            "select * from (values ('p-q'), ('r')) t(x), lateral string_to_table(x, '-') s;",
            &["p-q|p", "p-q|q", "r|r"],
        ),
        ("select s, length(s) from string_to_table('aa bbb', ' ') s;", &["aa|2", "bbb|3"]),
        ("select count(*) from string_to_table(repeat('x,', 1000), ',');", &["1001"]),
        ("select * from string_to_table('a b', ' ') where string_to_table = 'b';", &["b"]),
        ("select string_to_table('a', ',') from generate_series(1, 2);", &["a", "a"]),
        ("select normalize(x, nfkc) from (values (U&'\\FB01'), (U&'\\2460')) t(x);", &["fi", "1"]),
        ("select x is nfkd normalized from (values (U&'\\FB01'), ('a')) t(x);", &["f", "t"]),
        ("select pg_typeof(normalize('a')), pg_typeof('a' is normalized);", &["text|boolean"]),
        ("select string_to_table(1::text || ',2', ',');", &["1", "2"]),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(expected.len())), "{sql}");
        let rows: Vec<String> =
            messages[1..=expected.len()].iter().map(|m| text(data_row(m))).collect();
        assert_eq!(rows, expected, "{sql}");
    }
    for (sql, state, message) in [
        ("select normalize('a', nfx);", "42601", "syntax error at or near \"nfx\""),
        ("select normalize('a', 'NFC');", "42601", "syntax error at or near \"'NFC'\""),
        (
            "select 1 where string_to_table('a', ',') = 'a';",
            "0A000",
            "set-returning functions are not allowed in WHERE",
        ),
        ("select is_normalized('a', 'x');", "22023", "invalid normalization form: x"),
        ("select normalize('a', 'x');", "42601", "syntax error at or near \"'x'\""),
    ] {
        // An error of a kernel comes after the row description.
        let messages = client.query(sql);
        assert!(["EZ", "TEZ"].contains(&tags(&messages).as_str()), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_array_functions_resolve_their_types_and_give_the_values_of_postgres() {
    let dirs = Dirs::new("pgarrays");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The rows are the ones that PostgreSQL 19 gives, with a null as an empty string.
    let cases: [(&str, &[&str]); 16] = [
        ("select array_dims(array[1,2,3]), array_dims('{}'::int[]);", &["[1:3]|"]),
        (
            "select array_position(array['a','b','c'], 'b'), array_positions(array[1,2,1], 1);",
            &["2|{1,3}"],
        ),
        (
            "select array_position(array[1,2,3], 3, 2), array_position(array[1,2,3], 1, 2), array_position(array[1], 1.5);",
            &["3||"],
        ),
        (
            "select array_position(array[1,null], null::int), array_positions(array[null,1,null], null::int);",
            &["2|{1,3}"],
        ),
        (
            "select array_remove(array[1,2,1], 1), array_replace(array[1,2,1], 1, 9), array_replace(array[1,2], 2, null);",
            &["{2}|{9,2,9}|{1,NULL}"],
        ),
        ("select array_remove('{1}', 1), array_remove(array[1,null], null);", &["{}|{1}"]),
        (
            "select trim_array(array[1,2,3], 1), trim_array(array[1,2,3], 0), trim_array(array[1,2,3], 3);",
            &["{1,2}|{1,2,3}|{}"],
        ),
        (
            "select array_reverse(array[1,2,3]), array_sort(array[3,1,2]), array_sort(array[3,null,1], true), array_sort(array[3,null,1], false, true);",
            &["{3,2,1}|{1,2,3}|{NULL,3,1}|{NULL,1,3}"],
        ),
        (
            "select array_sample(array[1], 1), array_shuffle(array[1]), array_shuffle(array[]::int[]), cardinality(array_sample(array[1,2,3], 3));",
            &["{1}|{1}|{}|3"],
        ),
        (
            "select width_bucket(5, array[1,4,8]), width_bucket(0, array[1,2]), width_bucket(9.5, array[1,2.5,9.5]), width_bucket('b'::text, array['a','c']);",
            &["2|0|3|1"],
        ),
        (
            "select array_to_string(array[1,null,3], ','), array_to_string(array[1,null,3], ',', '*');",
            &["1,3|1,*,3"],
        ),
        (
            "select array_to_string(array[true,false,null], '-', 'n'), array_to_string(array[1.5::float8, 1e20, 'NaN'], ';');",
            &["t-f-n|1.5;1e+20;NaN"],
        ),
        (
            "select array_to_string('{}'::int[], ','), array_to_string(array[null::int], ','), array_to_string(array[null::int, 1], ',');",
            &["||1"],
        ),
        (
            "select array_to_string(array['a','b'], null, 'x'), array_to_string(null::int[], ',', 'x'), array_to_string(array[null,'b'], ',', null);",
            &["||b"],
        ),
        (
            "select array_to_string(array['2024-01-02'::date, null], '|', ''), array_to_string(array['\\x01ff'::bytea], ','), array_to_string(array[1.50::numeric], ',');",
            &["2024-01-02||\\x01ff|1.50"],
        ),
        (
            "select pg_typeof(array_reverse(array[1.5])), pg_typeof(array_positions(array['a'], 'a')), pg_typeof(array_to_string(array[1], ','));",
            &["numeric[]|integer[]|text"],
        ),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(expected.len())), "{sql}");
        let rows: Vec<String> =
            messages[1..=expected.len()].iter().map(|m| text(data_row(m))).collect();
        assert_eq!(rows, expected, "{sql}");
    }
    for (sql, state, message) in [
        (
            "select array_reverse('{1,2}');",
            "42804",
            "could not determine polymorphic type because input has type unknown",
        ),
        (
            "select array_to_string('{a,b}', ',');",
            "42804",
            "could not determine polymorphic type because input has type unknown",
        ),
        (
            "select array_remove(array[1], 'x');",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "select array_cat(array[1], array['a']);",
            "42883",
            "function array_cat(integer[], text[]) does not exist",
        ),
        (
            "select array_position(array[1,2], 2, null::int);",
            "22004",
            "initial position must not be null",
        ),
        (
            "select trim_array(array[1,2], 3);",
            "2202E",
            "number of elements to trim must be between 0 and 2",
        ),
        ("select array_sample(array[1,2], -1);", "22023", "sample size must be between 0 and 2"),
        (
            "select width_bucket(5, array[1,null]);",
            "22004",
            "thresholds array must not contain NULLs",
        ),
    ] {
        // An error of a kernel comes after the row description.
        let messages = client.query(sql);
        assert!(["EZ", "TEZ"].contains(&tags(&messages).as_str()), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_array_operators_resolve_as_postgres_resolves_them() {
    let dirs = Dirs::new("pgarrayops");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The rows are the ones that PostgreSQL 19 gives, with a null as an empty string.
    let cases: [(&str, &[&str]); 15] = [
        (
            "select array[1,2] || 3, 0 || array[1,2], array[1,2] || array[3,4], array[1] || null, null || array[1];",
            &["{1,2,3}|{0,1,2}|{1,2,3,4}|{1}|{1}"],
        ),
        (
            "select array[1,2] || '{3,4}', '{0}' || array[1,2], array[1.5] || 2, 2 || array[1.5];",
            &["{1,2,3,4}|{0,1,2}|{1.5,2}|{2,1.5}"],
        ),
        ("select '{1}' || 2;", &["{1}2"]),
        (
            "select array[]::int[] || array[1], array[1] || array[]::int[], null::int[] || null::int[];",
            &["{1}|{1}|"],
        ),
        ("select pg_typeof(array[1] || 2.5), array[1] || 2.5;", &["numeric[]|{1,2.5}"]),
        (
            "select array[1,2,2] @> array[2,1], array[1] @> array[]::int[], array[1,null] @> array[null::int], array[]::int[] <@ array[null::int];",
            &["t|t|f|t"],
        ),
        (
            "select array[null,2] && array[2], array[null::int] && array[null::int], array[]::int[] && array[]::int[], null::int[] @> array[1];",
            &["t|f|f|"],
        ),
        ("select array[1] @> '{1}', '{1,2}' <@ array[1,2,3], array['a'] && '{a}';", &["t|t|t"]),
        ("select array_append('{1}', 2);", &["{1,2}"]),
        (
            "select array_append(array[1], 2), array_prepend(0, array[1]), array_cat(array[1], array[2.5]), array_append(null::int[], null), array_cat(null::int[], array[1]);",
            &["{1,2}|{0,1}|{1,2.5}|{NULL}|{1}"],
        ),
        (
            "select array_cat(array[1], null), array_cat('{}'::int[], null::int[]), array_prepend(null, null::text[]);",
            &["{1}|{}|{NULL}"],
        ),
        ("select array['a','B'] @> array['b'];", &["f"]),
        (
            "select array[1.0] @> array[1.00], array['NaN'::float8] @> array['NaN'::float8];",
            &["t|t"],
        ),
        (
            "select array['a','b'] || array['c'], 'x' || 'y' || 1, array[true] && array[false, true];",
            &["{a,b,c}|xy1|t"],
        ),
        (
            "select pg_typeof(array[1] && array[2]), pg_typeof(0::int8 || array[1]);",
            &["boolean|bigint[]"],
        ),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(expected.len())), "{sql}");
        let rows: Vec<String> =
            messages[1..=expected.len()].iter().map(|m| text(data_row(m))).collect();
        assert_eq!(rows, expected, "{sql}");
    }
    for (sql, state, message) in [
        ("select array[1] || '3';", "22P02", "malformed array literal: \"3\""),
        (
            "select array['a'] || 'b', 'b' || array['a'], 'x' || 'y', 'x' || 1;",
            "22P02",
            "malformed array literal: \"b\"",
        ),
        (
            "select array[1] @> array[1.5];",
            "42883",
            "operator does not exist: integer[] @> numeric[]",
        ),
        ("select array[1] @> 1;", "42883", "operator does not exist: integer[] @> integer"),
        ("select 1 @> 2;", "42883", "operator does not exist: integer @> integer"),
        ("select 1 @@@ 2;", "42883", "operator does not exist: integer @@@ integer"),
        ("select '1' + '2';", "42725", "operator is not unique: unknown + unknown"),
        ("select date '2020-01-01' + '1';", "42725", "operator is not unique: date + unknown"),
    ] {
        // An error of the binder comes before a row description, and an error of a kernel after it.
        let messages = client.query(sql);
        assert!(["EZ", "TEZ"].contains(&tags(&messages).as_str()), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_collation_is_named_and_checked_as_postgres_does_it() {
    let dirs = Dirs::new("pgcollate");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The rows are the ones that PostgreSQL 19 gives, with a null as an empty string.
    let cases: [(&str, &[&str]); 17] = [
        ("select 'a' collate \"C\";", &["a"]),
        ("select lower('A' collate \"C\"), pg_typeof('a' collate \"C\");", &["a|unknown"]),
        ("select array_sort(array['b','A','a'] collate \"C\");", &["{A,a,b}"]),
        ("select 'a' < 'B' collate \"C\", 'a' collate \"C\" < 'B';", &["f|f"]),
        ("select upper('a' collate \"POSIX\"), 'a' collate \"default\";", &["A|a"]),
        ("select max(x collate \"C\") from (values ('b'),('A')) t(x);", &["b"]),
        (
            "select x from (values ('b'),('A'),('a')) t(x) order by x collate \"C\";",
            &["A", "a", "b"],
        ),
        ("select 'a' collate pg_catalog.\"C\";", &["a"]),
        (
            "select 'a'::varchar collate \"C\", 'a'::char(2) collate \"C\", 'a'::name collate \"C\", array['a'] collate \"C\";",
            &["a|a |a|{a}"],
        ),
        (
            "select pg_typeof('a'::varchar collate \"C\"), pg_typeof(array['a'] collate \"C\");",
            &["character varying|text[]"],
        ),
        (
            "select pg_typeof('a'::char(2) collate \"C\"), pg_typeof('a'::name collate \"C\");",
            &["character|name"],
        ),
        ("select 'a' collate \"C\" collate \"POSIX\";", &["a"]),
        ("select null collate \"C\";", &[""]),
        ("select x collate \"ucs_basic\" from (values ('b'),('A')) t(x) order by 1;", &["A", "b"]),
        ("select length('a' collate \"C\") = length('b' collate \"POSIX\");", &["t"]),
        (
            "select (select 'a' collate \"C\") = 'b' collate \"POSIX\", 'a' collate \"C\" collate \"POSIX\" = 'b' collate \"POSIX\";",
            &["f|f"],
        ),
        (
            "select 'a' collate pg_catalog.default, 'a' collate \"ucs_basic\", 'a' collate unicode, 'a' collate pg_unicode_fast;",
            &["a|a|a|a"],
        ),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(expected.len())), "{sql}");
        let rows: Vec<String> =
            messages[1..=expected.len()].iter().map(|m| text(data_row(m))).collect();
        assert_eq!(rows, expected, "{sql}");
    }
    let mismatch = "collation mismatch between explicit collations \"C\" and \"POSIX\"";
    for (sql, state, message, position) in [
        ("select 1 collate \"C\";", "42804", "collations are not supported by type integer", "10"),
        (
            "select true collate \"C\";",
            "42804",
            "collations are not supported by type boolean",
            "13",
        ),
        (
            "select 1::numeric collate \"C\";",
            "42804",
            "collations are not supported by type numeric",
            "19",
        ),
        (
            "select '{1}'::int[] collate \"C\";",
            "42804",
            "collations are not supported by type integer[]",
            "21",
        ),
        (
            "select 'a' collate \"nosuch\";",
            "42704",
            "collation \"nosuch\" for encoding \"UTF8\" does not exist",
            "12",
        ),
        (
            "select 'a' collate C;",
            "42704",
            "collation \"c\" for encoding \"UTF8\" does not exist",
            "12",
        ),
        ("select 'a' collate nosuch.\"C\";", "3F000", "schema \"nosuch\" does not exist", "12"),
        (
            "select 'a' collate x.y.z;",
            "0A000",
            "cross-database references are not implemented: x.y.z",
            "12",
        ),
        (
            "select 'a' collate a.b.c.d;",
            "42601",
            "improper qualified name (too many dotted names): a.b.c.d",
            "12",
        ),
        ("select 'a' collate \"C\" = 'b' collate \"POSIX\";", "42P21", mismatch, "30"),
        ("select 'a' collate \"C\" || 'b' collate \"POSIX\";", "42P21", mismatch, "31"),
        (
            "select case when true then 'a' collate \"C\" else 'b' collate \"POSIX\" end;",
            "42P21",
            mismatch,
            "53",
        ),
        ("select concat('a' collate \"C\", 'b' collate \"POSIX\");", "42P21", mismatch, "36"),
        ("select ('a' collate \"C\")::text || 'b' collate \"POSIX\";", "42P21", mismatch, "39"),
        ("select 'a' collate \"C\" in ('a' collate \"POSIX\");", "42P21", mismatch, "32"),
        (
            "select max(x collate \"C\") = 'b' collate \"POSIX\" from (values ('b')) t(x);",
            "42P21",
            mismatch,
            "33",
        ),
        ("select array['a' collate \"C\", 'b' collate \"POSIX\"];", "42P21", mismatch, "35"),
        ("select coalesce('a' collate \"C\", 'b' collate \"POSIX\");", "42P21", mismatch, "38"),
        ("select 'a' collate \"C\" like 'a' collate \"POSIX\";", "42P21", mismatch, "33"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        let error = &messages[0];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P').as_deref(), Some(position), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_case_of_text_follows_its_collation_as_postgres_does_it() {
    let dirs = Dirs::new("pgcase");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The rows are the ones that PostgreSQL 19 gives. The default collation of a database of
    // rudb is `C`, so a call with no collation maps only the ASCII letters, as `C` does.
    let cases: [(&str, &[&str]); 28] = [
        (
            "select upper('abc é ß'), lower('ABC É'), initcap('hELLO éa'), casefold('ABC ẞ');",
            &["ABC é ß|abc É|Hello éA|abc ẞ"],
        ),
        (
            "select upper('abc é ß' collate \"C\"), lower('ABC É' collate \"POSIX\"), initcap('hELLO éa' collate ucs_basic);",
            &["ABC é ß|abc É|Hello éA"],
        ),
        (
            "select upper('abc é ß ǆ' collate pg_c_utf8), lower('ΑΣ ẞ' collate pg_c_utf8), casefold('ẞ ABC' collate pg_c_utf8);",
            &["ABC É ß Ǆ|ασ ß|ß abc"],
        ),
        (
            "select initcap('hello wORLD foo_bar 1abc ǆa' collate pg_c_utf8), initcap('١a' collate pg_c_utf8);",
            &["Hello World Foo_Bar 1abc Ǆa|١A"],
        ),
        (
            "select upper('ß ŉ' collate pg_unicode_fast), casefold('ẞ ß' collate pg_unicode_fast), lower('ΑΣ ΑΣ.Α Σ' collate pg_unicode_fast);",
            &["SS ʼN|ss ss|ας ασ.α σ"],
        ),
        (
            "select initcap('ǆa ßa' collate pg_unicode_fast), initcap('١a' collate pg_unicode_fast);",
            &["ǅa Ssa|١a"],
        ),
        (
            "select lower('ΑΣ' collate pg_c_utf8), lower('ΑΣ0' collate pg_unicode_fast), lower('ΑΣ''Α' collate pg_unicode_fast);",
            &["ασ|ας0|ασ'α"],
        ),
        ("select upper(x collate pg_c_utf8) from (values ('é'), ('ß')) t(x);", &["É", "ß"]),
        (
            "select initcap('ǅungla x' collate pg_unicode_fast), initcap('ǅungla x' collate pg_c_utf8);",
            &["ǅungla X|Ǆungla X"],
        ),
        (
            "select upper('aé'::varchar collate pg_c_utf8), upper('aé'::char(3) collate pg_c_utf8), lower(name 'ÉA');",
            &["AÉ|AÉ|Éa"],
        ),
        ("select upper(null collate pg_c_utf8), pg_typeof(casefold('a'));", &["|text"]),
        (
            "select casefold('ﬃ' collate pg_unicode_fast), upper('ﬃ' collate pg_c_utf8), initcap('ﬃx' collate pg_unicode_fast);",
            &["ffi|ﬃ|Ffix"],
        ),
        (
            "select initcap('o''neil d''arcy' collate pg_c_utf8), initcap('o''neil' collate \"C\");",
            &["O'Neil D'Arcy|O'Neil"],
        ),
        // `ILIKE` matches the lower case of both sides, by the collation of the call.
        ("select 'ABC' ilike 'abc', 'abc' ilike 'A_C', 'É' ilike 'é';", &["t|t|f"]),
        (
            "select 'É' ilike 'é' collate \"C\", 'É' ilike 'é' collate pg_c_utf8, 'ÉCOLE' not ilike 'éc%' collate pg_unicode_fast;",
            &["f|t|f"],
        ),
        (
            "select 'ẞ' ilike 'ß' collate pg_c_utf8, 'ẞ' ilike 's%' collate pg_unicode_fast, 'İ' ilike 'i%' collate pg_unicode_fast;",
            &["t|f|t"],
        ),
        (
            "select 'ΑΣ' ilike 'ας' collate pg_unicode_fast, 'ΑΣ' ilike 'ασ' collate pg_unicode_fast, 'ΑΣ' ilike 'ασ' collate pg_c_utf8;",
            &["t|f|t"],
        ),
        (
            "select 'É_b' ilike 'é$_B' escape '$' collate pg_c_utf8, null::text ilike 'a' collate pg_c_utf8;",
            &["t|"],
        ),
        (
            "select x from (values ('école'), ('ÉCOLE'), ('ecole')) t(x) where x ilike 'É%' collate pg_c_utf8;",
            &["école", "ÉCOLE"],
        ),
        // A regular expression takes its character classes, its case folding and its word
        // boundaries from the collation of the call.
        (
            "select 'é' ~* 'É', 'é' collate pg_c_utf8 ~* 'É', 'ǅ' collate pg_c_utf8 ~* 'ǅ', 'ǆ' collate pg_c_utf8 ~* '[ǅ]', 'ss' ~* 'ß' collate pg_unicode_fast;",
            &["f|t|f|t|f"],
        ),
        (
            "select substring('1abé α2' from '[[:alpha:]]+'), substring('1abé α2' collate pg_c_utf8 from '[[:alpha:]]+');",
            &["ab|abé"],
        ),
        (
            "select substring('a٣12' collate pg_c_utf8 from '[[:digit:]]+'), substring('a٣12' collate pg_unicode_fast from '[[:digit:]]+');",
            &["12|٣12"],
        ),
        (
            "select substring('a$+!b' collate pg_c_utf8 from '[[:punct:]]+'), substring('a$+!b' collate pg_unicode_fast from '[[:punct:]]+');",
            &["$+!|!"],
        ),
        (
            "select regexp_replace('a b c' collate pg_c_utf8, '\\s', '_', 'g'), regexp_replace('a b', '\\s', '_', 'g');",
            &["a_b_c|a_b"],
        ),
        (
            "select regexp_match(',a_é,', '\\w+'), regexp_match(',a_é,' collate pg_c_utf8, '\\w+'), regexp_match('.a٣.' collate pg_unicode_fast, '\\w+');",
            &["{a_}|{a_é}|{a٣}"],
        ),
        (
            "select regexp_count('éb éb', '\\mb'), regexp_count('éb éb' collate pg_c_utf8, '\\mb'), regexp_count('éb éb', '\\yb\\y');",
            &["2|0|2"],
        ),
        (
            "select regexp_like('ÀÉ' collate pg_c_utf8, '^[à-é]+$', 'i'), regexp_like('ÀÉ', '^[à-é]+$', 'i'), regexp_split_to_array('aébÉc' collate pg_c_utf8, 'é', 'i');",
            &["t|f|{a,b,c}"],
        ),
        (
            "select x from (values ('é'), ('É'), ('e')) t(x) where x collate pg_c_utf8 ~ '^[[:lower:]]$';",
            &["é", "e"],
        ),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(expected.len())), "{sql}");
        let rows: Vec<String> =
            messages[1..=expected.len()].iter().map(|m| text(data_row(m))).collect();
        assert_eq!(rows, expected, "{sql}");
    }
    let mismatch = "collation mismatch between explicit collations \"C\" and \"pg_c_utf8\"";
    for (sql, state, message, position) in [
        (
            "select upper(x collate \"C\") = upper(x collate pg_c_utf8) from (values ('é')) t(x);",
            "42P21",
            mismatch,
            Some("39"),
        ),
        (
            "select upper(x collate \"C\" || 'é' collate pg_c_utf8) from (values ('é')) t(x);",
            "42P21",
            mismatch,
            Some("35"),
        ),
        ("select 'a' collate \"C\" ilike 'a' collate pg_c_utf8;", "42P21", mismatch, Some("34")),
        ("select upper('a' collate unicode);", "0A000", "ICU is not supported in this build", None),
        (
            "select 'a' ilike 'A' collate unicode;",
            "0A000",
            "ICU is not supported in this build",
            None,
        ),
        ("select 'a' collate \"C\" ~ 'a' collate pg_c_utf8;", "42P21", mismatch, Some("30")),
        ("select 'a' ~ 'A' collate unicode;", "0A000", "ICU is not supported in this build", None),
    ] {
        let messages = client.query(sql);
        let error = &messages[0];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P').as_deref(), position, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_column_carries_its_collation_as_an_implicit_one_as_postgres_does_it() {
    let dirs = Dirs::new("pgimplicit");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The rows are the ones that PostgreSQL 19 gives. A column of a subquery, a `WITH` query, a
    // `VALUES` list or a set operation keeps the collation of the expression that makes it.
    let cases: [(&str, &[&str]); 10] = [
        ("select lower(t), upper(t) from (select 'ÀB' collate pg_c_utf8 as t) s;", &["àb|ÀB"]),
        ("with w as (select 'ÀB' collate pg_c_utf8 as t) select lower(t) from w;", &["àb"]),
        (
            "with recursive r(t) as (select 'ÀB' collate pg_c_utf8 union all select t from r where false) select lower(t) from r;",
            &["àb"],
        ),
        (
            "select lower(x) from (values ('À' collate pg_c_utf8), ('É')) v(x) order by 1;",
            &["à", "é"],
        ),
        (
            "select lower(t) from (select t from (select 'ÀB' collate pg_c_utf8 as t) s0 group by t) s;",
            &["àb"],
        ),
        // An explicit collation beats an implicit one.
        (
            "select lower(t) from (select 'ÀB' collate pg_c_utf8 as t) s where t = 'ÀB' collate \"C\";",
            &["àb"],
        ),
        (
            "select lower(x) from (select 'À' collate pg_c_utf8 union select 'É') s(x) order by 1;",
            &["à", "é"],
        ),
        // The column of a set operation is implicit to the set operation that holds it.
        (
            "select x from (select 'a' collate \"C\" union select 'b' union select 'c' collate pg_c_utf8) s(x) order by 1;",
            &["a", "b", "c"],
        ),
        (
            "select a || b collate \"C\" from (select 'À' collate pg_c_utf8 as a, 'É' collate \"C\" as b) s order by 1;",
            &["ÀÉ"],
        ),
        // A column that the set operation could not give a collation takes none, so the constant
        // gives the default collation, which is `C` in rudb.
        (
            "select lower(x || 'a') from (select a from (select 'À' collate pg_c_utf8 as a) s1 union all select b from (select 'É' collate \"C\" as b) s2) s(x) order by 1;",
            &["Àa", "Éa"],
        ),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(expected.len())), "{sql}");
        let rows: Vec<String> =
            messages[1..=expected.len()].iter().map(|m| text(data_row(m))).collect();
        assert_eq!(rows, expected, "{sql}");
    }
    let explicit = "collation mismatch between explicit collations \"pg_c_utf8\" and \"C\"";
    let implicit = "collation mismatch between implicit collations \"pg_c_utf8\" and \"C\"";
    let pair = "(select 'À' collate pg_c_utf8 as a, 'É' collate \"C\" as b) s";
    let choose =
        "You can choose the collation by applying the COLLATE clause to one or both expressions.";
    let set = "Use the COLLATE clause to set the collation explicitly.";
    let undetermined =
        |what: &str| format!("could not determine which collation to use for {what}");
    for (sql, state, message, hint, position) in [
        (
            "select lower(x) from (select 'À' collate pg_c_utf8 union all select 'É' collate \"C\") s(x);".to_owned(),
            "42P21",
            explicit.to_owned(),
            None,
            Some("73"),
        ),
        (
            "select lower(x) from (select a from (select 'À' collate pg_c_utf8 as a) s1 union select b from (select 'É' collate \"C\" as b) s2) s(x);".to_owned(),
            "42P21",
            implicit.to_owned(),
            Some(choose),
            Some("89"),
        ),
        (format!("select a from {pair} order by a || b;"), "42P21", implicit.to_owned(), Some(choose), Some("89")),
        (format!("select a || b from {pair} group by 1;"), "42P21", implicit.to_owned(), Some(choose), Some("13")),
        (format!("select distinct a || b from {pair};"), "42P21", implicit.to_owned(), Some(choose), Some("22")),
        (format!("select lower(a || b) from {pair};"), "42P22", undetermined("lower() function"), Some(set), None),
        (format!("select a < b from {pair};"), "42P22", undetermined("string comparison"), Some(set), None),
        (format!("select a || b like 'a' from {pair};"), "42P22", undetermined("LIKE"), Some(set), None),
        (format!("select a || b ilike 'a' from {pair};"), "42P22", undetermined("ILIKE"), Some(set), None),
        (format!("select a || b ~ 'a' from {pair};"), "42P22", undetermined("regular expression"), Some(set), None),
        (
            "select lower(x) from (select a from (select 'À' collate pg_c_utf8 as a) s1 union all select b from (select 'É' collate \"C\" as b) s2) s(x);".to_owned(),
            "42P22",
            undetermined("lower() function"),
            Some(set),
            None,
        ),
    ] {
        let messages = client.query(&sql);
        let error = &messages[0];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message.as_str()), "{sql}");
        assert_eq!(error.field(b'H').as_deref(), hint, "{sql}");
        assert_eq!(error.field(b'P').as_deref(), position, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_subscript_of_an_array_is_coerced_and_bounded_as_postgres_does_it() {
    let dirs = Dirs::new("pgsubscripts");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let text = |row: Vec<Option<Vec<u8>>>| {
        row.into_iter()
            .map(|value| value.map_or(String::new(), |v| String::from_utf8(v).unwrap()))
            .collect::<Vec<_>>()
            .join("|")
    };
    // The rows are the ones that PostgreSQL 19 gives, with a null as an empty string.
    let cases: [(&str, &[&str]); 17] = [
        ("select (array[1,2,3])[null];", &[""]),
        ("select (array[1,2,3])[1:null];", &[""]),
        ("select (array['a','b'])[1.6];", &["b"]),
        ("select ('{1,2,3}'::int[])[-1:1];", &["{1}"]),
        (
            "select (array[1,2,3])[-1], (array[1,2,3])[-2:-1], (array[1,2,3])[-5:2], (array[1,2,3])[2:10], (array[1,2,3])[4:5];",
            &["|{}|{1,2}|{2,3}|{}"],
        ),
        (
            "select (array[1,2,3])['2'], (array[1,2,3])[2::int8], (array[1,2,3])[2.5::float8];",
            &["2|2|2"],
        ),
        (
            "select (array[1,2,3])[1:2.5], (array[1,2,3])[null:2], (array[1,2,3])[:null];",
            &["{1,2,3}||"],
        ),
        (
            "select pg_typeof((array[1,2,3])[1]), pg_typeof((array[1,2,3])[1:2]);",
            &["integer|integer[]"],
        ),
        ("select ('{a,b}'::text[])[2], (string_to_array('a,b', ','))[2];", &["b|b"]),
        ("select (null::int[])[1], (null::int[])[1:2];", &["|"]),
        (
            "select string_to_array('1 2', ' ')::int[], pg_typeof(string_to_array('1 2', ' ')::int8[]);",
            &["{1,2}|bigint[]"],
        ),
        ("select array['1.5', null]::float8[], array['t','f']::bool[];", &["{1.5,NULL}|{t,f}"]),
        ("select array[' 7 ']::int2[];", &["{7}"]),
        ("select (array[1,2,3])[1.5::float8];", &["2"]),
        ("select (array[1,2,3])[2::int8];", &["2"]),
        ("select array[1,2] = array[1,2], array[1] = '{1}', '{1,2}' <> array[1,2];", &["t|t|f"]),
        ("select (string_to_array('1,2,3', ','))[2:], (array[1,2,3])[:];", &["{2,3}|{1,2,3}"]),
    ];
    for (sql, expected) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), format!("T{}CZ", "D".repeat(expected.len())), "{sql}");
        let rows: Vec<String> =
            messages[1..=expected.len()].iter().map(|m| text(data_row(m))).collect();
        assert_eq!(rows, expected, "{sql}");
    }
    for (sql, state, message) in [
        ("select (array[1,2,3])['x'];", "22P02", "invalid input syntax for type integer: \"x\""),
        ("select (array[1,2,3])[true];", "42804", "array subscript must have type integer"),
        ("select (array[1,2,3])[3000000000];", "22003", "integer out of range"),
        (
            "select string_to_array('a b', ' ')::int[];",
            "22P02",
            "invalid input syntax for type integer: \"a\"",
        ),
        (
            "select (array[1,2,3])['1.5'];",
            "22P02",
            "invalid input syntax for type integer: \"1.5\"",
        ),
        (
            "select array_length('{1,2}', 1);",
            "42804",
            "could not determine polymorphic type because input has type unknown",
        ),
        (
            "select array_lower('{5}', 1);",
            "42804",
            "could not determine polymorphic type because input has type unknown",
        ),
        (
            "select cardinality('{1,2,3}');",
            "42804",
            "could not determine polymorphic type because input has type unknown",
        ),
    ] {
        // An error of the binder comes before a row description, and an error of a kernel after it.
        let messages = client.query(sql);
        assert!(["EZ", "TEZ"].contains(&tags(&messages).as_str()), "{sql}");
        let error = &messages[messages.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_parameter_in_a_call_gets_the_type_of_postgres() {
    let dirs = Dirs::new("unknowns");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // Each case has the parameter types and the column types of PostgreSQL.
    let cases: [(&str, &[u32], &[u32]); 13] = [
        ("select abs($1)", &[701], &[701]),
        ("select substr($1, $2)", &[25, 23], &[25]),
        ("select repeat($1, $2)", &[25, 23], &[25]),
        ("select lpad($1, 3)", &[25], &[25]),
        ("select coalesce($1, $2)", &[25, 25], &[25]),
        ("select nullif($1, $2)", &[25, 25], &[25]),
        ("select max($1)", &[25], &[25]),
        ("select sign($1)", &[701], &[701]),
        ("select round($1, 2)", &[1700], &[1700]),
        ("select mod($1, 2)", &[23], &[23]),
        ("select $1 + $2", &[23, 23], &[23]),
        ("select * from generate_series(1, $1)", &[23], &[23]),
        ("select generate_series(1, $1)", &[23], &[23]),
    ];
    for (sql, parameters, columns) in cases {
        client.parse("", sql, &[]);
        client.describe(Target::Statement, "");
        let messages = client.sync();
        assert_eq!(tags(&messages), "1tTZ", "{sql}");
        assert_eq!(parameter_types(&messages[1]), parameters, "{sql}");
        let shape: Vec<u32> = row_shape(&messages[2]).into_iter().map(|(_, oid, _)| oid).collect();
        assert_eq!(shape, columns, "{sql}");
    }
    // A parameter in a query that has no values for it is an undefined parameter.
    let messages = client.query("select * from generate_series(1, $1)");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42P02"));
    assert_eq!(messages[0].field(b'M').as_deref(), Some("there is no parameter $1"));
    server.stop().unwrap();
}

#[test]
fn a_numeric_parameter_keeps_its_type_and_all_its_digits() {
    let dirs = Dirs::new("numeric_param");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // A driver such as psycopg sends a decimal as a `numeric` parameter. The statement must give
    // the `numeric` column at Describe and at Execute, or the plan of the statement changes type.
    let long = "123456789012345678901234567890123456789012.5";
    for value in ["1.50", long, "NaN"] {
        client.parse("", "select $1", &[1700]);
        client.describe(Target::Statement, "");
        client.bind("", "", &[], &[Some(value.as_bytes())]);
        client.describe(Target::Portal, "");
        client.execute("", 0);
        let messages = client.sync();
        assert_eq!(tags(&messages), "1tT2TDCZ", "{value}");
        assert_eq!(parameter_types(&messages[1]), [1700]);
        assert_eq!(row_shape(&messages[2])[0].1, 1700);
        assert_eq!(row_shape(&messages[4])[0].1, 1700);
        assert_eq!(data_row(&messages[5]), [Some(value.as_bytes().to_vec())]);
    }
    // An integer column meets a `numeric` parameter as a `numeric`, and an insert rounds it.
    client.query("create table t (i int)");
    client.parse("", "insert into t values ($1)", &[1700]);
    client.bind("", "", &[], &[Some(b"1.5")]);
    client.execute("", 0);
    client.parse("", "select i, i + $1 from t where i = $2", &[1700, 1700]);
    client.bind("", "", &[], &[Some(b"0.25"), Some(b"2.0")]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12C12DCZ");
    assert_eq!(data_row(&messages[5]), [Some(b"2".to_vec()), Some(b"2.25".to_vec())]);
    server.stop().unwrap();
}

#[test]
fn a_mean_and_a_sum_of_a_numeric_are_numerics_with_their_digits() {
    let dirs = Dirs::new("numeric_mean");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table t (g int, i int, b bigint, d numeric(10,2), m numeric)");
    client.query(
        "insert into t values (1, 1, 9223372036854775807, 1.25, 1e30), \
         (1, 2, 9223372036854775807, 2.50, 3.14159), (2, null, null, null, null)",
    );
    // Each case has the text of PostgreSQL 19 and the column type, which is `numeric` for all.
    let cases = [
        ("select avg(i) from t", "1.5000000000000000"),
        ("select avg(b) from t", "9223372036854775807"),
        ("select avg(d) from t", "1.8750000000000000"),
        ("select sum(m) from t", "1000000000000000000000000000003.14159"),
        ("select avg(m) from t", "500000000000000000000000000001.57080"),
        ("select avg(i) from t where g = 2", ""),
        ("select avg(i) over (partition by g) from t order by g, i limit 1", "1.5000000000000000"),
    ];
    for (sql, text) in cases {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "TDCZ", "{sql}");
        assert_eq!(row_shape(&messages[0])[0].1, 1700, "{sql}");
        let wanted = (!text.is_empty()).then(|| text.as_bytes().to_vec());
        assert_eq!(data_row(&messages[1]), [wanted], "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_parameter_alone_in_the_select_list_has_its_declared_type() {
    let dirs = Dirs::new("declared_param");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // `"char"`, `name`, `oid` and `bpchar` have the logical type of another PostgreSQL type, so
    // the column must take the declared type and not the type of its values.
    client.parse("", "select $1, $2, $3, $4, $5", &[18, 19, 26, 1042, 705]);
    client.describe(Target::Statement, "");
    let values: [&[u8]; 5] = [b"a", b"x", b"42", b"ab", b"u"];
    client.bind("", "", &[], &values.map(Some));
    client.describe(Target::Portal, "");
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "1tT2TDCZ");
    // A parameter of the type `unknown` has no type, and one of no type is `text`.
    assert_eq!(parameter_types(&messages[1]), [18, 19, 26, 1042, 25]);
    let types = [18, 19, 26, 1042, 25];
    assert_eq!(row_shape(&messages[2]).iter().map(|c| c.1).collect::<Vec<_>>(), types);
    assert_eq!(row_shape(&messages[4]).iter().map(|c| c.1).collect::<Vec<_>>(), types);
    assert_eq!(data_row(&messages[5]), values.map(|v| Some(v.to_vec())));
    server.stop().unwrap();
}

#[test]
fn a_value_of_no_type_takes_the_type_of_a_bytea_or_of_the_elements_of_an_array() {
    let dirs = Dirs::new("bytea_array");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table t (b bytea, a int4[])");
    let cases: [(&str, &[u32], u32); 4] = [
        ("select b || $1 from t", &[17], 17),
        ("select $1 || b from t", &[17], 17),
        ("select array_append(a, $1) from t", &[23], 1007),
        ("select array_prepend($1, a) from t", &[23], 1007),
    ];
    for (sql, types, column) in cases {
        client.parse("", sql, &[]);
        client.describe(Target::Statement, "");
        let messages = client.sync();
        assert_eq!(tags(&messages), "1tTZ", "{sql}");
        assert_eq!(parameter_types(&messages[1]), types, "{sql}");
        assert_eq!(row_shape(&messages[2])[0].1, column, "{sql}");
    }
    // Two `bytea` values are joined as bytes, and a string literal next to one is a `bytea`.
    let sql = "select '\\xff'::bytea || '\\x00', array_append(array[1, 2], '3')";
    let messages = client.query(sql);
    assert_eq!(row_shape(&messages[0]).iter().map(|c| c.1).collect::<Vec<_>>(), [17, 1007]);
    assert_eq!(data_row(&messages[1]), [Some(b"\\xff00".to_vec()), Some(b"{1,2,3}".to_vec())]);
    server.stop().unwrap();
}

#[test]
fn the_storage_options_of_a_table_and_a_truncate_of_several_tables_are_those_of_pgbench() {
    let dirs = Dirs::new("pgbench");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // `pgbench -i` makes its tables with a fill factor and empties them with one truncate.
    let messages =
        client.query("create table a (i int) with (FillFactor=100, toast.vacuum_truncate)");
    assert_eq!(tags(&messages), "CZ");
    let messages = client.query("create table b with (fillfactor=50) as select 1 i");
    assert_eq!(tags(&messages), "CZ");
    let errors = [
        ("fillfactor=5", "22023", "value 5 out of bounds for option \"fillfactor\""),
        ("foo=1", "22023", "unrecognized parameter \"foo\""),
        ("x.y=1", "22023", "unrecognized parameter namespace \"x\""),
        ("oids=true", "0A000", "tables declared WITH OIDS are not supported"),
    ];
    for (options, code, message) in errors {
        let messages = client.query(&format!("create table c (i int) with ({options})"));
        assert_eq!(tags(&messages), "EZ", "{options}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{options}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{options}");
    }
    client.query("insert into a values (1)");
    // A table that is not there truncates none of them.
    let messages = client.query("truncate a, c");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(scalar(&mut client, "select count(*) from a"), "1");
    let messages = client.query("truncate a, b");
    assert_eq!(tags(&messages), "CZ");
    assert_eq!(messages[0].body, b"TRUNCATE TABLE\0");
    assert_eq!(
        scalar(&mut client, "select (select count(*) from a) + (select count(*) from b)"),
        "0"
    );
    server.stop().unwrap();
}

#[test]
fn now_is_the_start_of_the_transaction_and_the_clock_moves() {
    let dirs = Dirs::new("instants");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // A statement of a simple query starts when the server reads the message.
    let same = "select now() = statement_timestamp() and now() = transaction_timestamp()";
    assert_eq!(scalar(&mut client, same), "t");
    client.query("begin");
    let begun = scalar(&mut client, "select now()");
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(scalar(&mut client, "select now()"), begun);
    assert_eq!(scalar(&mut client, "select current_timestamp"), begun);
    assert_eq!(scalar(&mut client, "select statement_timestamp() > now()"), "t");
    assert_eq!(scalar(&mut client, "select clock_timestamp() >= statement_timestamp()"), "t");
    client.query("commit");
    assert_eq!(scalar(&mut client, "select now() > timestamptz '2020-01-01'"), "t");
    // The clock is read again for each row.
    let moved = "select count(distinct clock_timestamp()) > 1 from generate_series(1, 50000)";
    assert_eq!(scalar(&mut client, moved), "t");
    let messages =
        client.query("select pg_typeof(clock_timestamp()), pg_typeof(statement_timestamp())");
    assert_eq!(
        data_row(&messages[1]),
        [Some(b"timestamp with time zone".to_vec()), Some(b"timestamp with time zone".to_vec())]
    );
    server.stop().unwrap();
}

#[test]
fn fetch_first_is_a_limit_and_a_negative_count_is_the_error_of_postgres() {
    let dirs = Dirs::new("fetch_first");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // Hibernate and other ORMs write a limit in the words of the SQL standard.
    let sql =
        "select g from generate_series(1, 9) g order by g offset $1 rows fetch next $2 rows only";
    client.parse("", sql, &[]);
    client.describe(Target::Statement, "");
    client.bind("", "", &[], &[Some(b"2"), Some(b"3")]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "1tT2DDDCZ");
    assert_eq!(parameter_types(&messages[1]), [20, 20]);
    let rows: Vec<_> = messages[4..7].iter().map(data_row).collect();
    assert_eq!(rows, [[Some(b"3".to_vec())], [Some(b"4".to_vec())], [Some(b"5".to_vec())]]);
    let messages = client.query("select 1 fetch first 1 row only");
    assert_eq!(tags(&messages), "TDCZ");

    // PostgreSQL finds a negative count when the query runs, so the error has no position. rudb
    // finds a constant count when it binds the query, so no RowDescription goes before the error.
    for (sql, order, code, message) in [
        ("select 1 limit -1", "EZ", "2201W", "LIMIT must not be negative"),
        ("select 1 fetch first -1 rows only", "EZ", "2201W", "LIMIT must not be negative"),
        ("select 1 offset -1", "EZ", "2201X", "OFFSET must not be negative"),
        ("select 1 offset (select -1)", "TEZ", "2201X", "OFFSET must not be negative"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), order, "{sql}");
        let error = &messages[order.len() - 2];
        assert_eq!(error.field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P'), None, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_rows_before_an_error_go_before_the_error() {
    let dirs = Dirs::new("partial");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let sql = "select 10 / g from generate_series(3, -1, -1) g";
    let messages = client.query(sql);
    assert_eq!(tags(&messages), "TDDDEZ");
    let rows: Vec<_> = messages[1..4].iter().map(data_row).collect();
    assert_eq!(rows, [[Some(b"3".to_vec())], [Some(b"5".to_vec())], [Some(b"10".to_vec())]]);
    assert_eq!(messages[4].field(b'C').as_deref(), Some("22012"));
    // A limit that has its rows before the row that fails gives no error.
    let messages = client.query("select 10 / g from generate_series(3, -1, -1) g limit 2");
    assert_eq!(tags(&messages), "TDDCZ");
    // The extended flow sends the rows at Execute, and the error where the CommandComplete goes.
    client.parse("", sql, &[]);
    client.bind("", "", &[], &[]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12TDDDEZ");
    // With a row limit the portal stops at the limit, and the next Execute gives the rest and
    // then the error.
    client.query("begin");
    client.parse("", sql, &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 2);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12DDsDEZ");
    client.query("rollback");
    // A statement that fails at its first row sends its columns and no rows.
    assert_eq!(tags(&client.query("select 1 / g from generate_series(0, 2) g")), "TEZ");
    // An error of folding a constant comes when the query is planned, before its columns, and
    // on the extended flow at Bind.
    let sql = "select 1 / 0 + g from generate_series(1, 2) g where false";
    assert_eq!(tags(&client.query(sql)), "EZ");
    client.parse("", sql, &[]);
    client.bind("", "", &[], &[]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    assert_eq!(tags(&client.sync()), "1EZ");
    assert_eq!(tags(&client.query("select 1")), "TDCZ");
    server.stop().unwrap();
}

/// The rows of the messages and the command tag at the end, for a result too large to list.
fn counted(messages: &[Message]) -> (String, usize, Option<Vec<Option<Vec<u8>>>>) {
    let rows = messages.iter().filter(|m| m.tag == b'D').count();
    let last = messages.iter().rev().find(|m| m.tag == b'D').map(data_row);
    let shape = tags(messages).replace('D', "");
    (shape, rows, last)
}

#[test]
fn a_large_result_goes_out_while_the_query_runs() {
    let dirs = Dirs::new("stream");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table big as select i, 'row ' || i as t from range(0, 200000) r(i)");
    let last = |i: i64| Some(vec![Some(i.to_string().into_bytes())]);

    // The rows come in the order of the table, and the tag counts the rows that went out.
    let messages = client.query("select i from big");
    assert_eq!(counted(&messages), ("TCZ".to_owned(), 200_000, last(199_999)));
    assert_eq!(text(&messages[messages.len() - 2]), "SELECT 200000");

    // A client that does not read for a while holds the query, and then gets all the rows.
    client.send(&Frontend::Query(b"select i from big"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(counted(&client.until_ready()), ("TCZ".to_owned(), 200_000, last(199_999)));

    // The rows before an error go out before it, as in PostgreSQL.
    let messages = client.query("select i, 10 / (i - 199990) from big");
    let (shape, rows, _) = counted(&messages);
    assert_eq!((shape.as_str(), rows), ("TEZ", 199_990));
    assert_eq!(messages[messages.len() - 2].field(b'C').as_deref(), Some("22012"));

    // A query of more than one statement.
    let messages = client.query("select 1; select i from big; select 2");
    assert_eq!(counted(&messages).1, 200_002);
    assert_eq!(tags(&messages).chars().filter(|&tag| tag == 'C').count(), 3);

    // The extended flow, with the rows in binary.
    client.parse("", "select i from big", &[]);
    client.bind_with("", "", &[], &[], &[1]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    let messages = client.sync();
    let binary = Some(vec![Some(199_999i64.to_be_bytes().to_vec())]);
    assert_eq!(counted(&messages), ("12TCZ".to_owned(), 200_000, binary));

    // A portal with a row limit sends its rows in parts, and the rest at the next Execute.
    client.query("begin");
    client.parse("", "select i from big", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 1000);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(counted(&messages), ("12sCZ".to_owned(), 200_000, last(199_999)));
    assert_eq!(text(&messages[messages.len() - 2]), "SELECT 199000");
    client.query("rollback");

    // The error of the extended flow comes where the CommandComplete goes.
    client.parse("", "select i, 10 / (i - 199990) from big", &[]);
    client.bind("", "", &[], &[]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    let (shape, rows, _) = counted(&client.sync());
    assert_eq!((shape.as_str(), rows), ("12TEZ", 199_990));

    assert_eq!(tags(&client.query("select 1")), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn a_call_that_no_function_takes_is_the_error_of_postgres() {
    let dirs = Dirs::new("function");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for (sql, message, detail) in [
        (
            "select no_such(1, 'a', null, 2.5)",
            "function no_such(integer, unknown, unknown, numeric) does not exist",
            "There is no function of that name.",
        ),
        (
            "select upper(1)",
            "function upper(integer) does not exist",
            "No function of that name accepts the given argument types.",
        ),
        (
            "select abs(1, 2)",
            "function abs(integer, integer) does not exist",
            "No function of that name accepts the given number of arguments.",
        ),
        (
            "select upper('a', 'b')",
            "function upper(unknown, unknown) does not exist",
            "No function of that name accepts the given number of arguments.",
        ),
        // `concat` is variadic, and a variadic function takes at least the arguments it declares.
        (
            "select concat()",
            "function concat() does not exist",
            "No function of that name accepts the given number of arguments.",
        ),
        // `make_interval` has defaults for all seven, so only an eighth is too many.
        (
            "select make_interval(1, 2, 3, 4, 5, 6, 7.0, 8)",
            "function make_interval(integer, integer, integer, integer, integer, integer, \
             numeric, integer) does not exist",
            "No function of that name accepts the given number of arguments.",
        ),
        // A call that names a type is a cast only when the cast keeps the value or goes through
        // text, and not for a row to a string type.
        (
            "select int4(now())",
            "function int4(timestamp with time zone) does not exist",
            "No function of that name accepts the given argument types.",
        ),
        (
            "select text(row(1, 2))",
            "function text(record) does not exist",
            "No function of that name accepts the given argument types.",
        ),
        // `round` with a scale is only over `numeric`, and a `float8` does not cast to it with no
        // cast written.
        (
            "select round(1.5::float8, 1)",
            "function round(double precision, integer) does not exist",
            "No function of that name accepts the given argument types.",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42883"), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'D').as_deref(), Some(detail), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some("8"), "{sql}");
    }
    // Two of the functions take a call of two arguments of no type, and neither wins.
    let messages = client.query("select date_trunc('day', null)");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42725"));
    let message = "function date_trunc(unknown, unknown) is not unique";
    assert_eq!(messages[0].field(b'M').as_deref(), Some(message));
    let detail = "Could not choose a best candidate function.";
    assert_eq!(messages[0].field(b'D').as_deref(), Some(detail));
    assert_eq!(messages[0].field(b'P').as_deref(), Some("8"));
    server.stop().unwrap();
}

#[test]
fn a_call_that_names_a_type_is_a_cast_as_postgresql_has_it() {
    let dirs = Dirs::new("function-cast");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let code = error.field(b'C').unwrap_or_default();
            let text = error.field(b'M').unwrap_or_default();
            let place = error.field(b'P').unwrap_or_default();
            return format!("{code} {text} at {place}");
        }
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(|| "NULL".into(), |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        rows.join(";")
    };
    // The answers of the PostgreSQL 19 oracle.
    for (sql, expected) in [
        (
            "select int4('5'), text(5), float8('1.5'), bool('t'), date('2024-01-02')",
            "5|5|1.5|t|2024-01-02",
        ),
        ("select int8(5.6), int2(7), pg_typeof(int8(5.6)), pg_typeof(text(5))", "6|7|bigint|text"),
        ("select int4(null), pg_typeof(int4(null)), name('ab')", "NULL|integer|ab"),
        (
            "select text(true), int4(2.5::float8), int4(5::int8), float4(1), pg_typeof(float4(1))",
            "true|2|5|1|real",
        ),
        ("select int4(true), text(array[1, 2]), bpchar(5)", "1|{1,2}|5"),
        ("select date(timestamp '2024-01-02 03:04')", "2024-01-02"),
        ("select int8(x) + 1, text(x) from (values (1), (2)) t(x)", "2|1;3|2"),
        ("select int4('x')", "22P02 invalid input syntax for type integer: \"x\" at 13"),
    ] {
        assert_eq!(result(sql), expected, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn named_arguments_defaults_and_variadic_find_the_function_postgresql_finds() {
    let dirs = Dirs::new("function-named");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let mut text = format!(
                "{} {} at {}",
                error.field(b'C').unwrap_or_default(),
                error.field(b'M').unwrap_or_default(),
                error.field(b'P').unwrap_or_default()
            );
            for (field, name) in [(b'D', "DETAIL"), (b'H', "HINT")] {
                if let Some(value) = error.field(field) {
                    text.push_str(&format!(" {name}: {value}"));
                }
            }
            return text;
        }
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(|| "NULL".into(), |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        rows.join(";")
    };
    let names = "No function of that name accepts the given argument names.";
    let types = "No function of that name accepts the given argument types. \
                 HINT: You might need to add explicit type casts.";
    let count = "No function of that name accepts the given number of arguments.";
    let both = "In the closest available match, an argument was specified both positionally and \
                by name.";
    // The answers of the PostgreSQL 19 oracle.
    for (sql, expected) in [
        // A named argument goes to the parameter of that name, and the others take their defaults.
        ("select make_interval(days => 3)", "3 days".to_string()),
        ("select make_interval(days := 2, hours => 1)", "2 days 01:00:00".into()),
        ("select make_interval()", "00:00:00".into()),
        ("select make_interval(1, 2, days => 3)", "1 year 2 mons 3 days".into()),
        (
            "select make_interval(secs => 1.5), make_interval(weeks => 1, mins => 2), \
             pg_typeof(make_interval(secs => '1.5'))",
            "00:00:01.5|7 days 00:02:00|interval".into(),
        ),
        ("select make_interval(years := 1, months := null)", "NULL".into()),
        ("select make_interval(0, days => '2')", "2 days".into()),
        (
            "select x, make_interval(days => x) from (values (1), (2)) t(x)",
            "1|1 day;2|2 days".into(),
        ),
        ("select make_date(year => 2024, month => 2, day => 3)", "2024-02-03".into()),
        (
            "select make_interval(days => 'x')",
            "22P02 invalid input syntax for type integer: \"x\" at 30".into(),
        ),
        (
            "select make_interval(nope => 1)",
            format!(
                "42883 function make_interval(nope => integer) does not exist at 8 DETAIL: {names}"
            ),
        ),
        (
            "select left(str => 'abc', n => 2)",
            format!(
                "42883 function left(str => unknown, n => integer) does not exist at 8 DETAIL: {names}"
            ),
        ),
        (
            "select make_interval(1, years => 2)",
            format!(
                "42883 function make_interval(integer, years => integer) does not exist at 8 DETAIL: {both}"
            ),
        ),
        (
            "select make_interval(days => 3, 1)",
            "42601 positional argument cannot follow named argument at 33".into(),
        ),
        (
            "select make_interval(days => 1, days => 2)",
            "42601 argument name \"days\" used more than once at 33".into(),
        ),
        // The values of a variadic `any` that VARIADIC gives as one array.
        ("select concat(variadic array['a', 'b'])", "ab".into()),
        ("select concat_ws(',', variadic array['a', 'b', 'c'])", "a,b,c".into()),
        (
            "select concat(variadic array[1, 2]), concat(variadic array[true, null]), \
             concat(variadic array[1.5::float8, 2.50])",
            "12|t|1.52.5".into(),
        ),
        ("select concat(variadic array[[1, 2], [3, 4]])", "1234".into()),
        ("select concat_ws('-', variadic array[1, null, 2])", "1-2".into()),
        ("select concat(variadic null::text[])", "NULL".into()),
        (
            "select num_nulls(variadic null::int[]), num_nonnulls(variadic '{}'::int[]), \
             num_nulls(variadic array[1, null, null])",
            "NULL|0|2".into(),
        ),
        // `concat` writes each value by the output function of its type, which for a boolean is
        // not the cast to text.
        (
            "select concat(true), true::text, concat_ws(',', true, 1.5::float8)",
            "t|true|t,1.5".into(),
        ),
        ("select concat(variadic 'a')", "42804 VARIADIC argument must be an array at 24".into()),
        ("select concat(variadic null)", "42804 VARIADIC argument must be an array at 24".into()),
        (
            "select concat('x', variadic array['a'])",
            format!("42883 function concat(unknown, text[]) does not exist at 8 DETAIL: {count}"),
        ),
        (
            "select abs(variadic array[1])",
            format!("42883 function abs(integer[]) does not exist at 8 DETAIL: {types}"),
        ),
        (
            "select jsonb_extract_path_text(from_json => '{\"a\":1}', path_elems => array['a'])",
            "42883 function jsonb_extract_path_text(from_json => unknown, path_elems => text[]) \
             does not exist at 8 HINT: This call would be correct if the variadic array were \
             labeled VARIADIC and placed last."
                .into(),
        ),
    ] {
        assert_eq!(result(sql), expected, "{sql}");
    }
    let messages = client.query("select make_interval(days => 3)");
    let bytes = messages[0].decoded();
    let Backend::RowDescription(fields) = Backend::decode(&bytes).unwrap().unwrap().0 else {
        panic!("{:?}", messages[0]);
    };
    assert_eq!(fields[0].name, b"make_interval");
    server.stop().unwrap();
}

#[test]
fn a_literal_that_a_cast_cannot_read_has_its_place_and_an_overflow_has_none() {
    let dirs = Dirs::new("literal-place");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let overflow =
        "A field with precision 6, scale 2 must round to an absolute value less than 10^4.";
    for (sql, code, message, detail, context, place) in [
        (
            "select '1.5x'::numeric",
            "22P02",
            "invalid input syntax for type numeric: \"1.5x\"",
            None,
            None,
            Some("8"),
        ),
        (
            "select '{\"a\":1'::jsonb",
            "22P02",
            "invalid input syntax for type json",
            Some("The input string ended unexpectedly."),
            Some("JSON data, line 1: {\"a\":1"),
            Some("8"),
        ),
        (
            "select 1, '[1 2]'::json",
            "22P02",
            "invalid input syntax for type json",
            Some("Expected \",\" or \"]\", but found \"2\"."),
            Some("JSON data, line 1: [1 2..."),
            Some("11"),
        ),
        (
            "select 12345.6::numeric(6,2)",
            "22003",
            "numeric field overflow",
            Some(overflow),
            None,
            None,
        ),
        (
            "select '12345.6'::numeric(6,2)",
            "22003",
            "numeric field overflow",
            Some(overflow),
            None,
            None,
        ),
        (
            "select 123456::numeric(6,2)",
            "22003",
            "numeric field overflow",
            Some(overflow),
            None,
            None,
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'D').as_deref(), detail, "{sql}");
        assert_eq!(messages[0].field(b'W').as_deref(), context, "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), place, "{sql}");
    }
    // A literal keeps every digit of a `numeric`, and a `jsonb` is in its normal form.
    let digits = "12345678901234567890123456789012345678901234.5";
    let messages =
        client.query(&format!("select '{digits}'::numeric, '{{\"b\":1,\"a\":2}}'::jsonb"));
    let row: Vec<_> = data_row(&messages[1]).into_iter().map(Option::unwrap).collect();
    assert_eq!(row, [digits.as_bytes().to_vec(), br#"{"a": 2, "b": 1}"#.to_vec()]);
    server.stop().unwrap();
}

#[test]
fn create_unlogged_and_a_serial_column() {
    let dirs = Dirs::new("serial");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query("create unlogged table u (a int); insert into u values (1)");
    assert_eq!(tags(&messages), "CCZ");
    // A serial column is an integer with a sequence the table owns, and the file keeps both.
    let messages = client.query("create table s (id serial, b text)");
    assert_eq!(tags(&messages), "CZ");
    client.query("insert into s (b) values ('x'), ('y')");
    let messages = client.query("select nextval('s_id_seq')");
    assert_eq!(data_row(&messages[1]), [Some(b"3".to_vec())]);
    let messages = client.query("insert into s (id) values (null)");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("23502"));
    server.stop().unwrap();
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("insert into s (b) values ('z')");
    let messages = client.query("select id from s order by id");
    let ids: Vec<_> = messages[1..4].iter().map(|row| data_row(row)[0].clone().unwrap()).collect();
    assert_eq!(ids, [b"1".to_vec(), b"2".to_vec(), b"4".to_vec()]);
    // The table owns the sequence, so a drop of the table drops it.
    client.query("drop table s");
    let messages = client.query("select nextval('s_id_seq')");
    assert_eq!(tags(&messages), "EZ");
    let messages = client.query("create temp table d (id serial default 4)");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42601"));
    server.stop().unwrap();
}

#[test]
fn the_transaction_rules_of_postgres() {
    let dirs = Dirs::new("implicit");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table t (i integer)");
    let count = |client: &mut Client| {
        let messages = client.query("select count(*) from t");
        String::from_utf8(data_row(&messages[1])[0].clone().unwrap()).unwrap()
    };
    let warning = |message: &Message| {
        assert_eq!(message.field(b'S').as_deref(), Some("WARNING"));
        (message.field(b'C').unwrap(), message.field(b'M').unwrap())
    };
    let no_transaction = ("25P01".to_owned(), "there is no transaction in progress".to_owned());

    // A query of more than one statement runs in one transaction, and an error rolls it back.
    let messages = client.query("insert into t values (1); select nope");
    assert_eq!((tags(&messages).as_str(), messages[2].body.as_slice()), ("CEZ", &b"I"[..]));
    assert_eq!(count(&mut client), "0");

    // A COMMIT in it commits with a warning, and the next statements run in a new transaction.
    let messages =
        client.query("insert into t values (1); commit; insert into t values (2); select nope");
    assert_eq!(tags(&messages), "CNCCEZ");
    assert_eq!(warning(&messages[1]), no_transaction);
    assert_eq!(count(&mut client), "1");

    // A BEGIN in it makes the transaction a block, without a warning.
    let messages = client.query("select 1; begin; select 2");
    assert_eq!((tags(&messages).as_str(), messages[7].body.as_slice()), ("TDCCTDCZ", &b"T"[..]));
    let messages = client.query("begin");
    assert_eq!(tags(&messages), "NCZ");
    assert_eq!(
        warning(&messages[0]),
        ("25001".to_owned(), "there is already a transaction in progress".to_owned())
    );
    client.query("rollback");

    // COMMIT and ROLLBACK without a transaction give a warning and their tag.
    for sql in ["commit", "rollback"] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "NCZ");
        assert_eq!(warning(&messages[0]), no_transaction);
        assert_eq!(text(&messages[1]), sql.to_uppercase());
    }

    // In a failed block each statement gives 25P02 until the block ends.
    client.query("begin");
    client.query("select nope");
    let messages = client.query("select 1");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("25P02"));
    assert_eq!(
        messages[0].field(b'M').as_deref(),
        Some("current transaction is aborted, commands ignored until end of transaction block")
    );
    assert_eq!(text(&client.query("commit")[0]), "ROLLBACK");

    // The grammar reads all of a query before any of it runs, as `pg_parse_query` does. So a
    // syntax error in a later statement stops the first one, and in a failed block a syntax error
    // comes before 25P02.
    let messages = client.query("insert into t values (9); selec 2");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42601"));
    assert_eq!(count(&mut client), "1");
    client.query("begin");
    client.query("select nope");
    let messages = client.query("selec 1");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42601"));
    let messages = client.query("copy t from stdin (format nope)");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("25P02"));
    let messages = client.query("select $$a;b$$, E'x\\';' ; select 2");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("25P02"));
    assert_eq!(text(&client.query("rollback")[0]), "ROLLBACK");
    // A dollar quote and an escape string hold a semicolon, and empty statements are skipped.
    let messages = client.query("select $$a;b$$, E'x\\';' ;;; select 2 -- tail");
    assert_eq!(tags(&messages), "TDCTDCZ");

    // In the extended flow the transaction ends at Sync, and an error rolls back all the
    // statements after the last Sync.
    client.parse("", "insert into t values (3)", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    client.parse("", "select nope from", &[]);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12CEZ");
    assert_eq!(count(&mut client), "1");
    client.parse("", "insert into t values (3)", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    client.parse("c", "commit", &[]);
    client.bind("", "c", &[], &[]);
    client.execute("", 0);
    client.parse("", "select nope from", &[]);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12C12NCEZ");
    assert_eq!(count(&mut client), "2");
    client.parse("", "insert into t values (4)", &[]);
    client.bind("", "", &[], &[]);
    client.execute("", 0);
    assert_eq!(tags(&client.sync()), "12CZ");
    assert_eq!(count(&mut client), "3");

    // A query or a change to the data that names a column or a table that is not there fails at
    // Parse, so Bind and Execute are skipped until Sync. A statement of another kind is bound
    // only when it runs, so its Parse succeeds.
    for sql in ["select nope from t", "insert into nope values (1)", "delete from t where nope"] {
        client.parse("", sql, &[]);
        client.bind("", "", &[], &[]);
        client.execute("", 0);
        assert_eq!(tags(&client.sync()), "EZ", "{sql}");
    }
    client.parse("", "create view v as select nope from t", &[]);
    assert_eq!(tags(&client.sync()), "1Z");
    server.stop().unwrap();
}

#[test]
fn the_startup_refusals() {
    let dirs = Dirs::new("refusals");
    let mut config = dirs.config();
    config.set("max_connections", "1").unwrap();
    let server = Server::start(config).unwrap();

    // There is no TLS, so an SSLRequest gets N and the startup goes on in clear text.
    let mut first = Client::unix(&server);
    first.packet(&Packet::SslRequest);
    let mut answer = [0u8; 1];
    first.socket.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
    connect(&mut first, PROTOCOL_3_0);

    let mut second = Client::unix(&server);
    second.startup(PROTOCOL_3_0, "postgres");
    let messages = second.rest();
    assert_eq!(tags(&messages), "E");
    assert_eq!(messages[0].field(b'S').as_deref(), Some("FATAL"));
    assert_eq!(messages[0].field(b'C').as_deref(), Some("53300"));
    assert_eq!(messages[0].field(b'M').as_deref(), Some("sorry, too many clients already"));
    first.send(&Frontend::Terminate);
    assert!(first.rest().is_empty());

    for (database, sqlstate, message) in [
        ("nope", "3D000", "database \"nope\" does not exist"),
        ("a/b", "3D000", "database \"a/b\" does not exist"),
        ("template0", "55000", "database \"template0\" is not currently accepting connections"),
    ] {
        // The session that ended can still be in the registry for a moment.
        let messages = loop {
            let mut client = Client::unix(&server);
            client.startup(PROTOCOL_3_0, database);
            let messages = client.rest();
            if messages[0].field(b'C').as_deref() != Some("53300") {
                break messages;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(tags(&messages), "RE", "{database}");
        assert_eq!(messages[1].field(b'C').as_deref(), Some(sqlstate));
        assert_eq!(messages[1].field(b'M').as_deref(), Some(message));
    }
    server.stop().unwrap();
}

/// The `ParameterStatus` messages of a list, as `name=value`.
/// After 100 ms with no input, a session gives back the stack pages and the input buffer that
/// its last statement used. The statements after that get new pages and a new buffer.
#[test]
fn a_session_that_was_idle_runs_deep_and_long_statements() {
    let dirs = Dirs::new("idle");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::tcp(&server);
    connect(&mut client, PROTOCOL_3_2);
    let deep = format!("select {}1{}", "(".repeat(100), ")".repeat(100));
    let long = format!("select '{}'", "x".repeat(100_000));
    for _ in 0..3 {
        assert_eq!(scalar(&mut client, &deep), "1");
        assert_eq!(scalar(&mut client, &long).len(), 100_000);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(scalar(&mut client, "select 2"), "2");
        std::thread::sleep(Duration::from_millis(250));
    }
    server.stop().unwrap();
}

fn statuses(messages: &[Message]) -> Vec<String> {
    let status = |m: &Message| text(m).replacen('\0', "=", 1);
    messages.iter().filter(|m| m.tag == b'S').map(status).collect()
}

/// The value of the one row and the one column of a query.
fn scalar(client: &mut Client, sql: &str) -> String {
    let messages = client.query(sql);
    let row = messages.iter().find(|m| m.tag == b'D').unwrap();
    String::from_utf8(data_row(row)[0].clone().unwrap()).unwrap()
}

#[test]
fn the_roles_of_postgres() {
    let dirs = Dirs::new("roles");
    let server = Server::start(dirs.config()).unwrap();
    let mut admin = Client::unix(&server);
    connect(&mut admin, PROTOCOL_3_0);
    for sql in [
        "create role ra login connection limit 1",
        "create role rb",
        "create user rc createrole password 'pw'",
        "alter role rb rename to rd",
    ] {
        assert_eq!(tags(&admin.query(sql)), "CZ", "{sql}");
    }
    let error = |messages: &[Message]| {
        let error = messages.iter().find(|m| m.tag == b'E').unwrap();
        (error.field(b'C').unwrap(), error.field(b'M').unwrap())
    };
    let pair = |sqlstate: &str, message: &str| (sqlstate.to_owned(), message.to_owned());
    assert_eq!(error(&admin.query("create role ra")), pair("42710", "role \"ra\" already exists"));
    assert_eq!(
        error(&admin.query("create role pg_x")),
        pair("42939", "role name \"pg_x\" is reserved")
    );

    // The roles that cannot log in, and the limit of connections of a role.
    let mut first = Client::unix(&server);
    first.startup_as(PROTOCOL_3_0, "ra", "postgres");
    assert_eq!(tags(&first.until_ready()).chars().last(), Some('Z'));
    for (user, sqlstate, message) in [
        ("nope", "28000", "role \"nope\" does not exist"),
        ("rd", "28000", "role \"rd\" is not permitted to log in"),
        ("ra", "53300", "too many connections for role \"ra\""),
    ] {
        let mut client = Client::unix(&server);
        client.startup_as(PROTOCOL_3_0, user, "postgres");
        let messages = client.rest();
        assert_eq!(tags(&messages), "RE", "{user}");
        assert_eq!(messages[1].field(b'S').as_deref(), Some("FATAL"));
        assert_eq!(error(&messages), pair(sqlstate, message));
    }

    // SET ROLE changes the current user and `is_superuser`, and not the session user.
    let messages = admin.query("set role ra");
    assert_eq!(statuses(&messages), ["is_superuser=off"]);
    assert_eq!(scalar(&mut admin, "select current_user"), "ra");
    assert_eq!(scalar(&mut admin, "select session_user"), "rpg");
    assert_eq!(statuses(&admin.query("reset role")), ["is_superuser=on"]);
    assert_eq!(error(&admin.query("set role nope")), pair("22023", "role \"nope\" does not exist"));

    // A role that is not a superuser cannot become another role, and the error aborts the block,
    // which undoes the settings of the block at once.
    assert_eq!(
        error(&first.query("set role rpg")),
        pair("42501", "permission denied to set role \"rpg\"")
    );
    first.query("begin");
    first.query("set application_name = 'x'");
    let messages = first.query("set session authorization rpg");
    assert_eq!(
        error(&messages),
        pair("42501", "permission denied to set session authorization \"rpg\"")
    );
    assert_eq!(statuses(&messages), ["application_name=t"]);
    assert_eq!(messages.last().unwrap().body, b"E");
    first.query("rollback");

    // SET SESSION AUTHORIZATION reports the user first and then `is_superuser`.
    let messages = admin.query("set session authorization ra");
    assert_eq!(statuses(&messages), ["session_authorization=ra", "is_superuser=off"]);
    assert_eq!(scalar(&mut admin, "select session_user"), "ra");
    admin.query("reset session authorization");
    assert_eq!(tags(&admin.query("drop role rd, rc")), "CZ");
    assert_eq!(error(&admin.query("drop role rd")), pair("42704", "role \"rd\" does not exist"));
    assert_eq!(tags(&admin.query("drop role if exists rd")), "NCZ");
    server.stop().unwrap();
}

#[test]
fn alter_database_set_and_alter_role_set_keep_values_for_later_sessions() {
    let dirs = Dirs::new("db-role-settings");
    let server = Server::start(dirs.config()).unwrap();
    let mut admin = Client::unix(&server);
    connect(&mut admin, PROTOCOL_3_0);
    for (sql, expected) in [
        ("create role ru login", "CZ"),
        ("create database dt owner ru", "CZ"),
        ("alter database dt set work_mem = '5MB'", "CZ"),
        ("alter role ru set work_mem = '6MB'", "CZ"),
        ("alter role all set statement_timeout = '7s'", "CZ"),
        ("alter role ru in database dt set datestyle = sql, dmy", "CZ"),
        ("alter role ru in database dt set my.thing to 1", "CZ"),
        ("alter role ru set role = 'nosuch'", "NCZ"),
        ("alter role ru reset role", "CZ"),
        ("alter role ru set role = 'nosuch'", "NCZ"),
    ] {
        assert_eq!(tags(&admin.query(sql)), expected, "{sql}");
    }
    let error = |messages: &[Message]| {
        let error = messages.iter().find(|m| m.tag == b'E').unwrap();
        (error.field(b'C').unwrap(), error.field(b'M').unwrap())
    };
    let pair = |sqlstate: &str, message: &str| (sqlstate.to_owned(), message.to_owned());
    for (sql, sqlstate, message) in [
        ("alter database nope set work_mem = '1MB'", "3D000", "database \"nope\" does not exist"),
        (
            "alter database dt set nonexistent = 1",
            "42704",
            "unrecognized configuration parameter \"nonexistent\"",
        ),
        (
            "alter database dt reset nonexistent",
            "42704",
            "unrecognized configuration parameter \"nonexistent\"",
        ),
        (
            "alter database dt set transaction isolation level serializable",
            "42704",
            "unrecognized configuration parameter \"TRANSACTION\"",
        ),
        (
            "alter database dt set work_mem = 'x'",
            "22023",
            "invalid value for parameter \"work_mem\": \"x\"",
        ),
        ("alter database dt set work_mem = 1, 2", "22023", "SET work_mem takes only one argument"),
        (
            "alter database dt set log_connections = on",
            "55P02",
            "parameter \"log_connections\" cannot be set after connection start",
        ),
        ("alter database dt set catalog 'x'", "0A000", "current database cannot be changed"),
        (
            "alter role all in database nope set work_mem = '1MB'",
            "3D000",
            "database \"nope\" does not exist",
        ),
        ("alter role nope set work_mem = '1MB'", "42704", "role \"nope\" does not exist"),
        ("alter role pg_x set work_mem = '1MB'", "42939", "role name \"pg_x\" is reserved"),
    ] {
        assert_eq!(error(&admin.query(sql)), pair(sqlstate, message), "{sql}");
    }

    // A session of the role in the database starts with the most specific value of each
    // parameter, after a WARNING for a value that does not apply.
    let mut user = Client::unix(&server);
    user.startup_as(PROTOCOL_3_0, "ru", "dt");
    let messages = user.until_ready();
    let warning = messages.iter().find(|m| m.tag == b'N').unwrap();
    assert_eq!(warning.field(b'S').as_deref(), Some("WARNING"));
    assert_eq!(warning.field(b'M').as_deref(), Some("role \"nosuch\" does not exist"));
    assert!(statuses(&messages).contains(&"DateStyle=SQL, DMY".to_owned()));
    assert_eq!(scalar(&mut user, "show work_mem"), "6MB");
    assert_eq!(scalar(&mut user, "show statement_timeout"), "7s");
    assert_eq!(scalar(&mut user, "show my.thing"), "1");
    user.query("set work_mem = '1MB'");
    user.query("reset work_mem");
    assert_eq!(scalar(&mut user, "show work_mem"), "6MB", "RESET goes to the stored value");

    // The checks of who may keep a value.
    for (sql, sqlstate, message) in [
        ("alter role all set work_mem = '1MB'", "42501", "permission denied to alter setting"),
        ("alter role rpg set work_mem = '1MB'", "42501", "permission denied to alter role"),
        (
            "alter database postgres set work_mem = '1MB'",
            "42501",
            "must be owner of database postgres",
        ),
        (
            "alter database dt set log_min_messages = 'debug1'",
            "42501",
            "permission denied to set parameter \"log_min_messages\"",
        ),
        (
            "alter database dt set my.other = 1",
            "42501",
            "permission denied to set parameter \"my.other\"",
        ),
    ] {
        assert_eq!(error(&user.query(sql)), pair(sqlstate, message), "{sql}");
    }
    assert_eq!(tags(&user.query("alter role ru set work_mem = '2MB'")), "CZ");
    assert_eq!(tags(&user.query("alter database dt reset all")), "CZ");

    // A dropped database and a dropped role take their values with them.
    drop(user);
    for sql in ["drop database dt with (force)", "drop role ru"] {
        assert_eq!(tags(&admin.query(sql)), "CZ", "{sql}");
    }
    let file = std::fs::read_to_string(dirs.root.join("data/global/db_role_settings")).unwrap();
    assert_eq!(file, "rudb db_role_settings 1\nsetting\t0\t0\tstatement_timeout=7s\n");
    server.stop().unwrap();
}

#[test]
fn the_databases_of_postgres() {
    let dirs = Dirs::new("databases");
    let server = Server::start(dirs.config()).unwrap();
    let mut admin = Client::unix(&server);
    connect(&mut admin, PROTOCOL_3_0);
    let error = |messages: &[Message]| {
        let error = messages.iter().find(|m| m.tag == b'E').unwrap();
        (error.field(b'C').unwrap(), error.field(b'M').unwrap())
    };
    let pair = |sqlstate: &str, message: &str| (sqlstate.to_owned(), message.to_owned());
    assert_eq!(tags(&admin.query("create database src")), "CZ");
    assert_eq!(
        error(&admin.query("create database src")),
        pair("42P04", "database \"src\" already exists")
    );
    assert_eq!(
        error(&admin.query("select 1; create database d1")),
        pair("25001", "CREATE DATABASE cannot run inside a transaction block")
    );

    // A new database is a copy of its template, with the changes up to now.
    let mut client = Client::unix(&server);
    client.startup(PROTOCOL_3_0, "src");
    client.until_ready();
    client.query("create table t (a int)");
    client.query("insert into t values (42)");
    client.send(&Frontend::Terminate);
    assert!(client.rest().is_empty());
    let messages = loop {
        let messages = admin.query("create database copy template src");
        // The session that ended can still be in the registry for a moment.
        if messages.iter().all(|m| m.tag != b'E') || error(&messages).0 != "55006" {
            break messages;
        }
    };
    assert_eq!(tags(&messages), "CZ");
    let mut copy = Client::unix(&server);
    copy.startup(PROTOCOL_3_0, "copy");
    copy.until_ready();
    assert_eq!(scalar(&mut copy, "select a from t"), "42");

    // DROP DATABASE ... WITH (FORCE) ends the other sessions on the database.
    assert_eq!(tags(&admin.query("drop database copy with (force)")), "CZ");
    let messages = copy.rest();
    assert_eq!(tags(&messages), "E");
    assert_eq!(messages[0].field(b'S').as_deref(), Some("FATAL"));
    assert_eq!(messages[0].field(b'C').as_deref(), Some("57P01"));

    // A database that does not take connections, and the catalog after a restart.
    assert_eq!(tags(&admin.query("alter database src allow_connections false")), "CZ");
    server.stop().unwrap();
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    client.startup(PROTOCOL_3_0, "src");
    let messages = client.rest();
    assert_eq!(tags(&messages), "RE");
    assert_eq!(
        error(&messages),
        pair("55000", "database \"src\" is not currently accepting connections")
    );
    let mut client = Client::unix(&server);
    client.startup(PROTOCOL_3_0, "copy");
    assert_eq!(error(&client.rest()), pair("3D000", "database \"copy\" does not exist"));
    server.stop().unwrap();

    // With `auto_create_database`, a missing database is a new copy of `template1`.
    let mut config = dirs.config();
    config.set("auto_create_database", "on").unwrap();
    let server = Server::start(config).unwrap();
    let mut client = Client::unix(&server);
    client.startup(PROTOCOL_3_0, "auto");
    assert_eq!(tags(&client.until_ready()).chars().last(), Some('Z'));
    client.send(&Frontend::Terminate);
    assert!(client.rest().is_empty());
    let mut admin = Client::unix(&server);
    connect(&mut admin, PROTOCOL_3_0);
    let messages = loop {
        let messages = admin.query("drop database auto");
        if messages.iter().all(|m| m.tag != b'E') || error(&messages).0 != "55006" {
            break messages;
        }
    };
    assert_eq!(tags(&messages), "CZ");
    server.stop().unwrap();
}

#[test]
fn the_configuration_files_and_a_reload() {
    let dirs = Dirs::new("conf");
    let file = dirs.root.join("data/postgresql.conf");
    let sample = std::fs::read_to_string(&file).unwrap();
    assert!(
        sample.contains("\nmax_connections = 100                   # (change requires restart)\n")
    );
    let write = |lines: &str| std::fs::write(&file, format!("{sample}{lines}")).unwrap();
    write("work_mem = 8MB\nmy.custom = 'x'\nmax_connections = 50\n");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    assert_eq!(scalar(&mut client, "show work_mem"), "8MB");
    assert_eq!(scalar(&mut client, "show my.custom"), "x");
    assert_eq!(scalar(&mut client, "show max_connections"), "50");
    assert_eq!(tags(&client.query("set lock_timeout = '1s'")), "CZ");

    // A value that the session set stays, and the others take the new values of the files.
    // A parameter that leaves the files goes back to its default.
    write("work_mem = 16MB\nlock_timeout = 5s\nmax_connections = 60\n");
    server.reload();
    assert_eq!(scalar(&mut client, "show work_mem"), "16MB");
    assert_eq!(scalar(&mut client, "show lock_timeout"), "1s");
    assert_eq!(scalar(&mut client, "show max_connections"), "50");
    assert_eq!(scalar(&mut client, "show my.custom"), "");
    assert_eq!(tags(&client.query("reset lock_timeout")), "CZ");
    assert_eq!(scalar(&mut client, "show lock_timeout"), "5s");

    // A file with an error changes nothing.
    write("work_mem = 32MB\nnosuch = 1\n");
    server.reload();
    assert_eq!(scalar(&mut client, "show work_mem"), "16MB");
    write("");
    server.reload();
    assert_eq!(scalar(&mut client, "show work_mem"), "4MB");
    let mut other = Client::unix(&server);
    connect(&mut other, PROTOCOL_3_0);
    assert_eq!(scalar(&mut other, "show lock_timeout"), "0");
    assert_eq!(scalar(&mut other, "show config_file"), file.display().to_string());
    server.stop().unwrap();
}

#[test]
fn a_cancel_request_stops_the_statement() {
    let dirs = Dirs::new("cancel");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    let (pid, key) = connect(&mut client, PROTOCOL_3_2);
    assert_eq!(key.len(), 32);

    // A wrong key cancels nothing.
    let mut wrong = key.clone();
    wrong[0] ^= 1;
    let mut canceler = Client::tcp(&server);
    canceler.packet(&Packet::Cancel(Cancel { pid, key: &wrong }));
    assert!(canceler.rest().is_empty());
    assert_eq!(tags(&client.query("select 1")), "TDCZ");

    client.send(&Frontend::Query(b"select sum(i * i) from range(100000000000) r(i)"));
    std::thread::sleep(Duration::from_millis(300));
    let mut canceler = Client::tcp(&server);
    canceler.packet(&Packet::Cancel(Cancel { pid, key: &key }));
    assert!(canceler.rest().is_empty());
    let messages = client.until_ready();
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("57014"));
    assert_eq!(error.field(b'M').as_deref(), Some("canceling statement due to user request"));
    assert_eq!(tags(&client.query("select 1")), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn pg_sleep_waits_and_a_cancel_request_stops_it() {
    let dirs = Dirs::new("pg_sleep");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    let (pid, key) = connect(&mut client, PROTOCOL_3_2);
    let started = std::time::Instant::now();
    let messages = client.query("select pg_sleep(0.2), pg_typeof(pg_sleep(-1))");
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(tags(&messages), "TDCZ");
    assert_eq!(data_row(&messages[1]), [Some(Vec::new()), Some(b"void".to_vec())]);

    client.send(&Frontend::Query(b"select pg_sleep(60)"));
    std::thread::sleep(Duration::from_millis(300));
    let mut canceler = Client::tcp(&server);
    canceler.packet(&Packet::Cancel(Cancel { pid, key: &key }));
    assert!(canceler.rest().is_empty());
    let messages = client.until_ready();
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("57014"));
    assert_eq!(tags(&client.query("select 1")), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn statement_timeout_cancels_a_statement_that_runs_too_long() {
    let dirs = Dirs::new("stmttimeout");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    let (pid, key) = connect(&mut client, PROTOCOL_3_2);
    assert_eq!(tags(&client.query("set statement_timeout = '300ms'")), "CZ");
    // A wait, a recursive query that does not end, and a statement of the extended protocol, as
    // PostgreSQL 19 cancels them.
    let timed_out = |messages: &[Message], what: &str| {
        let error = messages.iter().find(|m| m.tag == b'E').unwrap_or_else(|| panic!("{what}"));
        assert_eq!(error.field(b'C').as_deref(), Some("57014"), "{what}");
        assert_eq!(
            error.field(b'M').as_deref(),
            Some("canceling statement due to statement timeout"),
            "{what}"
        );
    };
    for sql in [
        "select pg_sleep(30)",
        "with recursive t(n) as (values ('01'::text) union select n || '10' from t \
         where n < '100') select count(*) from t",
    ] {
        let started = std::time::Instant::now();
        timed_out(&client.query(sql), sql);
        assert!(started.elapsed() < Duration::from_secs(10), "{sql}");
    }
    client.parse("", "select pg_sleep($1::float8)", &[]);
    client.bind("", "", &[], &[Some(b"30")]);
    client.execute("", 0);
    timed_out(&client.sync(), "the extended protocol");
    // With no limit the statement runs, and a cancel request still stops a statement of the
    // extended protocol.
    assert_eq!(tags(&client.query("reset statement_timeout")), "CZ");
    assert_eq!(tags(&client.query("select pg_sleep(0.4)")), "TDCZ");
    client.parse("", "select pg_sleep($1::float8)", &[]);
    client.bind("", "", &[], &[Some(b"60")]);
    client.execute("", 0);
    client.send(&Frontend::Sync);
    std::thread::sleep(Duration::from_millis(300));
    let mut canceler = Client::tcp(&server);
    canceler.packet(&Packet::Cancel(Cancel { pid, key: &key }));
    assert!(canceler.rest().is_empty());
    let messages = client.until_ready();
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("57014"));
    assert_eq!(error.field(b'M').as_deref(), Some("canceling statement due to user request"));
    assert_eq!(tags(&client.query("select 1")), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn a_stop_ends_each_session_and_removes_the_files() {
    let dirs = Dirs::new("stop");
    let server = Server::start(dirs.config()).unwrap();
    let socket = server.sockets()[0].clone();
    assert!(socket.exists());
    // A second server cannot use the same data directory.
    let error = Server::start(dirs.config()).unwrap_err();
    assert!(error.starts_with("lock file"), "{error}");

    let mut idle = Client::unix(&server);
    connect(&mut idle, PROTOCOL_3_0);
    assert_eq!(
        tags(&idle.query("create table kept (i integer); insert into kept values (7)")),
        "CCZ"
    );
    let mut busy = Client::tcp(&server);
    connect(&mut busy, PROTOCOL_3_0);
    busy.send(&Frontend::Query(b"select sum(i * i) from range(100000000000) r(i)"));
    std::thread::sleep(Duration::from_millis(200));
    server.stop().unwrap();
    // The statement that the stop ends sends no cancel error, only the FATAL, as in PostgreSQL.
    for client in [&mut idle, &mut busy] {
        let messages = client.rest();
        assert_eq!(tags(&messages), "E");
        let error = &messages[0];
        assert_eq!(error.field(b'S').as_deref(), Some("FATAL"));
        assert_eq!(error.field(b'C').as_deref(), Some("57P01"));
    }
    assert!(!socket.exists());
    assert!(!dirs.root.join("data/rudb-server.pid").exists());

    // The table is in the file, so a new server sees it.
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query("select i from kept");
    assert_eq!(tags(&messages), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn a_smart_shutdown_waits_for_the_sessions() {
    let dirs = Dirs::new("smart");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    assert_eq!(tags(&client.query("begin; create table t (i integer)")), "CCZ");
    server.request(Shutdown::Smart);
    // A new session gets the refusal, and the session that runs goes on.
    let mut late = Client::unix(&server);
    late.startup(PROTOCOL_3_0, "postgres");
    let messages = late.rest();
    assert_eq!(tags(&messages), "E");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("57P03"));
    assert_eq!(messages[0].field(b'M').as_deref(), Some("the database system is shutting down"));
    assert!(!server.finished());
    assert_eq!(tags(&client.query("insert into t values (1); commit")), "CCZ");
    assert_eq!(scalar(&mut client, "select count(*) from t"), "1");
    // A second smart request does nothing.
    server.request(Shutdown::Smart);
    assert!(!server.finished());
    client.send(&Frontend::Terminate);
    assert!(client.rest().is_empty());
    for _ in 0..100 {
        if server.finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(server.finished());
    server.stop().unwrap();

    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    assert_eq!(scalar(&mut client, "select count(*) from t"), "1");
    server.stop().unwrap();
}

#[test]
fn an_immediate_shutdown_warns_and_keeps_the_commits() {
    let dirs = Dirs::new("immediate");
    let server = Server::start(dirs.config()).unwrap();
    let mut idle = Client::unix(&server);
    connect(&mut idle, PROTOCOL_3_0);
    assert_eq!(
        tags(&idle.query("create table kept (i integer); insert into kept values (7)")),
        "CCZ"
    );
    assert_eq!(tags(&idle.query("begin; insert into kept values (8)")), "CCZ");
    let mut busy = Client::unix(&server);
    connect(&mut busy, PROTOCOL_3_0);
    busy.send(&Frontend::Query(b"select sum(i * i) from range(100000000000) r(i)"));
    std::thread::sleep(Duration::from_millis(200));
    server.request(Shutdown::Immediate);
    // A weaker request does not take over.
    server.request(Shutdown::Fast);
    server.stop().unwrap();
    for client in [&mut idle, &mut busy] {
        let messages = client.rest();
        assert_eq!(tags(&messages), "N");
        assert_eq!(messages[0].field(b'S').as_deref(), Some("WARNING"));
        assert_eq!(messages[0].field(b'C').as_deref(), Some("57P01"));
        assert_eq!(
            messages[0].field(b'M').as_deref(),
            Some("terminating connection due to immediate shutdown command")
        );
    }
    assert!(!dirs.root.join("data/rudb-server.pid").exists());

    // The server did not write the file, and the next start reads the journal.
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    assert_eq!(scalar(&mut client, "select string_agg(i::text, ',') from kept"), "7");
    server.stop().unwrap();
}

/// The crypto provider of the build, as in the server.
#[cfg(feature = "tls-aws-lc")]
fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

#[cfg(all(feature = "tls-ring", not(feature = "tls-aws-lc")))]
fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

impl Socket for rustls::StreamOwned<rustls::ClientConnection, TcpStream> {}

/// A TLS client that trusts the test CA, with the ALPN protocols `alpn`.
fn tls_client(alpn: &[&[u8]]) -> std::sync::Arc<rustls::ClientConfig> {
    use rustls_pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    let ca = include_bytes!("tls/ca.crt");
    roots.add(rustls_pki_types::CertificateDer::from_pem_slice(ca).unwrap()).unwrap();
    let mut config = rustls::ClientConfig::builder_with_provider(provider().into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    std::sync::Arc::new(config)
}

/// Runs the TLS handshake of the client on `socket`. An error is the end of the connection.
fn tls_handshake(
    socket: TcpStream,
    alpn: &[&[u8]],
) -> std::io::Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(tls_client(alpn), name).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, socket);
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock)?;
    }
    Ok(stream)
}

#[test]
fn tls_with_an_ssl_request_and_with_direct_tls() {
    use std::os::unix::fs::PermissionsExt;
    let dirs = Dirs::new("tls");
    let data = dirs.root.join("data");
    std::fs::write(data.join("server.crt"), include_bytes!("tls/server.crt")).unwrap();
    std::fs::write(data.join("server.key"), include_bytes!("tls/server.key")).unwrap();
    let mut config = dirs.config();
    config.ssl = true;
    // A key that other users can read stops the start, as in PostgreSQL.
    let key = data.join("server.key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = Server::start(config.clone()).unwrap_err();
    assert!(error.starts_with("private key file \"server.key\" has group or world access"));
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = Server::start(config).unwrap();

    // SSLRequest, `S`, the handshake, and then the startup over TLS. A client that sends no ALPN
    // is fine on this path.
    let mut socket = TcpStream::connect(server.addresses()[0]).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut bytes = Vec::new();
    Packet::SslRequest.encode(&mut bytes);
    socket.write_all(&bytes).unwrap();
    let mut answer = [0u8; 1];
    socket.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"S");
    let stream = tls_handshake(socket, &[]).unwrap();
    let mut client = Client { socket: Box::new(stream), input: Vec::new() };
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query("show ssl");
    assert_eq!(tags(&messages), "TDCZ");
    assert_eq!(data_row(&messages[1]), vec![Some(b"on".to_vec())]);
    // A result larger than the TLS records and the flush size comes through whole.
    let messages = client.query("select repeat('x', 300000)");
    assert_eq!(data_row(&messages[1])[0].as_ref().map(Vec::len), Some(300_000));
    client.send(&Frontend::Terminate);
    assert!(client.rest().is_empty());

    // Direct TLS with ALPN. A later SSLRequest gets `N`.
    let socket = TcpStream::connect(server.addresses()[0]).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let stream = tls_handshake(socket, &[b"postgresql"]).unwrap();
    assert_eq!(stream.conn.alpn_protocol(), Some(&b"postgresql"[..]));
    let mut client = Client { socket: Box::new(stream), input: Vec::new() };
    client.packet(&Packet::SslRequest);
    let mut answer = [0u8; 1];
    client.socket.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
    connect(&mut client, PROTOCOL_3_2);
    let messages = client.query("select 1");
    assert_eq!(tags(&messages), "TDCZ");

    // Direct TLS without ALPN: the handshake ends, and then the server closes the connection.
    let socket = TcpStream::connect(server.addresses()[0]).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let stream = tls_handshake(socket, &[]).unwrap();
    let mut client = Client { socket: Box::new(stream), input: Vec::new() };
    client.startup(PROTOCOL_3_0, "postgres");
    assert!(client.rest().is_empty());

    // Clear text that comes with the SSLRequest is an error over TLS.
    let mut socket = TcpStream::connect(server.addresses()[0]).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut bytes = Vec::new();
    Packet::SslRequest.encode(&mut bytes);
    bytes.extend_from_slice(b"junk");
    socket.write_all(&bytes).unwrap();
    socket.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"S");
    let stream = tls_handshake(socket, &[]).unwrap();
    let mut client = Client { socket: Box::new(stream), input: Vec::new() };
    let messages = client.rest();
    assert_eq!(tags(&messages), "E");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("08P01"));
    assert_eq!(
        messages[0].field(b'M').as_deref(),
        Some("received unencrypted data after SSL request")
    );

    // There is no TLS on a Unix socket.
    let mut client = Client::unix(&server);
    client.packet(&Packet::SslRequest);
    client.socket.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query("show ssl_library");
    assert_eq!(data_row(&messages[1]), vec![Some(b"rustls".to_vec())]);
    server.stop().unwrap();
}

#[test]
fn advisory_locks_are_held_by_the_session_or_the_transaction() {
    let dirs = Dirs::new("advisory");
    let server = Server::start(dirs.config()).unwrap();
    let mut first = Client::unix(&server);
    connect(&mut first, PROTOCOL_3_0);
    let mut second = Client::unix(&server);
    connect(&mut second, PROTOCOL_3_0);
    let value = |messages: &[Message]| {
        let row = messages.iter().find(|m| m.tag == b'D').map(data_row).unwrap();
        String::from_utf8(row[0].clone().unwrap()).unwrap()
    };
    let try_lock = |client: &mut Client, key: &str| {
        value(&client.query(&format!("select pg_try_advisory_lock({key})")))
    };

    // A lock that gives void sends an empty value of type void.
    let messages = first.query("select pg_advisory_lock(42)");
    assert_eq!(tags(&messages), "TDCZ");
    assert_eq!(row_shape(&messages[0])[0].1, 2278);
    assert_eq!(value(&messages), "");
    assert_eq!(try_lock(&mut second, "42"), "f");
    // The pair of keys (0, 42) is not the same lock as the key 42.
    assert_eq!(try_lock(&mut second, "0, 42"), "t");
    let messages = second.query("set lock_timeout = 50; select pg_advisory_lock(42)");
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("55P03"));

    // A release of a lock that the session does not hold gives a warning and false.
    let messages = second.query("select pg_advisory_unlock(42)");
    assert_eq!(tags(&messages), "TNDCZ");
    assert_eq!(messages[1].field(b'C').as_deref(), Some("01000"));
    assert_eq!(value(&messages), "f");

    // A lock of the transaction level ends with the transaction, also after an error.
    first.query("begin");
    first.query("select pg_advisory_xact_lock(7)");
    assert_eq!(try_lock(&mut second, "7"), "f");
    first.query("select 1 / 0");
    first.query("rollback");
    assert_eq!(try_lock(&mut second, "7"), "t");
    assert_eq!(value(&second.query("select pg_advisory_unlock(7)")), "t");

    // A lock of the session level stays after an error, and ends with the session.
    assert_eq!(tags(&first.query("select 1 / 0")), "EZ");
    assert_eq!(try_lock(&mut second, "42"), "f");
    first.send(&Frontend::Terminate);
    drop(first);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while try_lock(&mut second, "42") == "f" {
        assert!(std::time::Instant::now() < deadline, "the lock stays after the session ended");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(tags(&second.query("select pg_advisory_unlock_all()")), "TDCZ");
    server.stop().unwrap();
}

#[test]
fn prepare_and_parse_share_one_namespace() {
    let dirs = Dirs::new("prepare");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let rows = |messages: &[Message]| -> Vec<String> {
        let rows = messages.iter().filter(|m| m.tag == b'D').map(data_row);
        rows.map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap()).collect()
    };
    let error = |messages: &[Message]| {
        let error = messages.iter().find(|m| m.tag == b'E').unwrap();
        (error.field(b'C').unwrap(), error.field(b'M').unwrap())
    };

    // `PREPARE` takes a list of types, and `EXECUTE` casts each value to its type.
    assert_eq!(text(&client.query("prepare q(int) as select $1 * 2")[0]), "PREPARE");
    let messages = client.query("execute q('21')");
    assert_eq!(tags(&messages), "TDCZ");
    assert_eq!(rows(&messages), ["42"]);
    assert_eq!(text(&messages[2]), "SELECT 1");
    let messages = client.query("execute q('x')");
    assert_eq!(error(&messages).0, "22P02");
    assert_eq!(messages[0].field(b'P').as_deref(), Some("11"));
    let messages = client.query("execute q");
    assert_eq!(error(&messages).1, "wrong number of parameters for prepared statement \"q\"");
    assert_eq!(messages[0].field(b'D').as_deref(), Some("Expected 1 parameters but got 0."));
    let messages = client.query("prepare q as select 2");
    assert_eq!(
        error(&messages),
        ("42P05".to_owned(), "prepared statement \"q\" already exists".to_owned())
    );
    let messages = client.query("prepare c as create table c (a int)");
    assert_eq!(
        error(&messages),
        ("42601".to_owned(), "syntax error at or near \"create\"".to_owned())
    );
    let messages = client.query("prepare c(nosuchtype) as select $1");
    assert_eq!(
        error(&messages),
        ("42704".to_owned(), "type \"nosuchtype\" does not exist".to_owned())
    );

    // A statement of `PREPARE` binds, and a statement of `Parse` runs by `EXECUTE`.
    client.bind_with("", "q", &[], &[Some(b"5")], &[1]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "2DCZ");
    assert_eq!(data_row(&messages[1])[0].as_deref(), Some(&10i32.to_be_bytes()[..]));
    client.parse("s", "select 'parsed'", &[]);
    assert_eq!(tags(&client.sync()), "1Z");
    assert_eq!(rows(&client.query("execute s")), ["parsed"]);
    assert_eq!(error(&client.query("prepare s as select 1")).0, "42P05");

    // `EXECUTE` on the extended flow gives the rows of the statement in the formats of `Bind`.
    client.parse("", "execute q(4)", &[]);
    client.describe(Target::Statement, "");
    client.bind_with("", "", &[], &[], &[1]);
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "1tT2DCZ");
    assert_eq!(parameter_types(&messages[1]), [0u32; 0]);
    assert_eq!(data_row(&messages[4])[0].as_deref(), Some(&8i32.to_be_bytes()[..]));

    // `DEALLOCATE` removes a statement of either kind, and `ALL` keeps the unnamed one.
    assert_eq!(text(&client.query("deallocate s")[0]), "DEALLOCATE");
    client.bind("", "s", &[], &[]);
    let messages = client.sync();
    assert_eq!(
        error(&messages),
        ("26000".to_owned(), "prepared statement \"s\" does not exist".to_owned())
    );
    assert_eq!(error(&client.query("deallocate s")).0, "26000");
    client.parse("", "select 'unnamed'", &[]);
    client.parse("n", "select 'named'", &[]);
    assert_eq!(tags(&client.sync()), "11Z");
    assert_eq!(text(&client.query("deallocate prepare all")[0]), "DEALLOCATE ALL");
    assert_eq!(error(&client.query("execute q(1)")).0, "26000");
    client.bind("", "n", &[], &[]);
    assert_eq!(error(&client.sync()).0, "26000");

    // In a failed block the three statements fail as any other.
    client.query("prepare q(int) as select $1");
    client.query("begin");
    client.query("select 1/0");
    for sql in ["prepare z as select 1", "execute q(1)", "deallocate q"] {
        assert_eq!(error(&client.query(sql)).0, "25P02", "{sql}");
    }
    client.query("rollback");
    assert_eq!(rows(&client.query("prepare m as values (7); execute m; deallocate m")), ["7"]);
    server.stop().unwrap();
}

#[test]
fn a_cursor_moves_and_ends_as_in_postgresql() {
    let dirs = Dirs::new("cursors");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let rows = |messages: &[Message]| -> Vec<String> {
        let rows = messages.iter().filter(|m| m.tag == b'D').map(data_row);
        rows.map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap()).collect()
    };
    let code =
        |messages: &[Message]| messages.iter().find(|m| m.tag == b'E').and_then(|e| e.field(b'C'));
    client.query("create table t(a int)");
    client.query("insert into t select g from generate_series(1, 5) g");

    // Outside of a block only a cursor `WITH HOLD` can open.
    assert_eq!(
        code(&client.query("declare c cursor for select a from t")).as_deref(),
        Some("25P01")
    );
    client.query("begin");
    let messages = client.query("declare c cursor for select a from t order by a");
    assert_eq!(text(&messages[0]), "DECLARE CURSOR");
    let messages = client.query("fetch 2 c");
    assert_eq!(tags(&messages), "TDDCZ");
    assert_eq!(rows(&messages), ["1", "2"]);
    assert_eq!(rows(&client.query("fetch last c")), ["5"]);
    assert_eq!(rows(&client.query("fetch backward 2 c")), ["4", "3"]);
    assert_eq!(text(&client.query("move forward all c")[0]), "MOVE 2");
    assert_eq!(rows(&client.query("fetch absolute 2 c")), ["2"]);
    assert_eq!(code(&client.query("declare c cursor for select 1")).as_deref(), Some("42P03"));
    client.query("rollback");

    // A cursor without `SCROLL` on a plan that cannot run backward only moves forward.
    client.query("begin");
    client.query("declare c cursor for select count(*) from t");
    let messages = client.query("fetch prior c");
    assert_eq!(code(&messages).as_deref(), Some("55000"));
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(
        error.field(b'H').as_deref(),
        Some("Declare it with SCROLL option to enable backward scan.")
    );
    client.query("rollback");

    // A cursor `WITH HOLD` stays after a commit, and goes after a rollback of its transaction.
    client.query("begin");
    client.query("declare h cursor with hold for select a from t order by a");
    client.query("commit");
    assert_eq!(rows(&client.query("fetch 2 h")), ["1", "2"]);
    client.query("begin");
    client.query("declare g cursor with hold for select a from t order by a");
    client.query("rollback");
    assert_eq!(code(&client.query("fetch g")).as_deref(), Some("34000"));
    assert_eq!(rows(&client.query("fetch h")), ["3"]);
    assert_eq!(text(&client.query("close all")[0]), "CLOSE CURSOR ALL");
    assert_eq!(code(&client.query("close h")).as_deref(), Some("34000"));

    // On the extended flow a `FETCH` takes the formats of its portal.
    client.query("begin");
    client.query("declare c cursor for select a from t order by a");
    client.parse("f", "fetch 2 c", &[]);
    client.bind_with("", "f", &[], &[], &[1]);
    client.describe(Target::Portal, "");
    client.execute("", 0);
    let messages = client.sync();
    assert_eq!(tags(&messages), "12TDDCZ");
    assert_eq!(row_shape(&messages[2])[0].2, 1);
    assert_eq!(data_row(&messages[3])[0].as_deref(), Some(&1i32.to_be_bytes()[..]));
    client.query("rollback");
    server.stop().unwrap();
}

#[test]
fn a_condition_must_be_a_boolean_as_in_postgresql() {
    let dirs = Dirs::new("condition");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table t (a int, b text, c bool)");
    client.query("insert into t values (1, 'x', true), (0, 'y', null)");
    // The errors and the positions of the PostgreSQL 19 oracle. The position is the first token
    // of the condition.
    for (sql, message, position) in [
        (
            "select 1 from t where a",
            "argument of WHERE must be type boolean, not type integer",
            "23",
        ),
        (
            "select case when 1 then 2 end",
            "argument of CASE/WHEN must be type boolean, not type integer",
            "18",
        ),
        (
            "select 1 from t having 1",
            "argument of HAVING must be type boolean, not type integer",
            "24",
        ),
        (
            "select 1 from t join t s on s.a",
            "argument of JOIN/ON must be type boolean, not type integer",
            "29",
        ),
        ("select 1 from t where b", "argument of WHERE must be type boolean, not type text", "23"),
        (
            "select 1 from t where c and a",
            "argument of AND must be type boolean, not type integer",
            "29",
        ),
        (
            "select 1 from t where not a",
            "argument of NOT must be type boolean, not type integer",
            "27",
        ),
        (
            "select 1 from t where a or c",
            "argument of OR must be type boolean, not type integer",
            "23",
        ),
        (
            "select a is true from t",
            "argument of IS TRUE must be type boolean, not type integer",
            "8",
        ),
        (
            "select a is not false from t",
            "argument of IS NOT FALSE must be type boolean, not type integer",
            "8",
        ),
        (
            "select a is unknown from t",
            "argument of IS UNKNOWN must be type boolean, not type integer",
            "8",
        ),
        (
            "select 1 where null::int",
            "argument of WHERE must be type boolean, not type integer",
            "16",
        ),
        (
            "select 1 where 1.5::float8",
            "argument of WHERE must be type boolean, not type double precision",
            "16",
        ),
        (
            "select 1 from t where a + 1",
            "argument of WHERE must be type boolean, not type integer",
            "23",
        ),
        (
            "select 1 from t where now()",
            "argument of WHERE must be type boolean, not type timestamp with time zone",
            "23",
        ),
        (
            "select count(*) filter (where b || 'z') from t",
            "argument of FILTER must be type boolean, not type text",
            "31",
        ),
        (
            "select sum(a) filter (where a + 1) over () from t",
            "argument of FILTER must be type boolean, not type integer",
            "29",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42804"), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(position), "{sql}");
    }
    // A string literal is read as a boolean, and a null is no row.
    for (sql, value) in [
        ("select count(*) from t where 'yes'", "2"),
        ("select count(*) from t where null", "0"),
        ("select count(*) from t where c", "1"),
        ("select count(*) from t where c is not unknown and 'on'", "1"),
        ("select case when 'true' then 1 end", "1"),
        ("select count(*) filter (where 'yes') from t", "2"),
        ("select count(*) filter (where null) from t", "0"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    let messages = client.query("select count(*) from t where 'x'");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("22P02"));
    assert_eq!(
        messages[0].field(b'M').as_deref(),
        Some("invalid input syntax for type boolean: \"x\"")
    );
    assert_eq!(messages[0].field(b'P').as_deref(), Some("30"));
    server.stop().unwrap();
}

#[test]
fn the_values_of_a_case_a_coalesce_and_an_array_take_the_common_type_of_postgresql() {
    let dirs = Dirs::new("common-type");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    client.query("create table w (a int, b bigint, d numeric(10,2))");
    client.query("insert into w values (1, 2, 1.25), (3, null, null)");
    // The values and the errors of the PostgreSQL 19 oracle. A string literal and a NULL take the
    // type of the other values, and a string literal is read with the input function of it.
    for (sql, value) in [
        ("select coalesce(1, '2') + 1", "2"),
        ("select array[1, '2']", "{1,2}"),
        ("select greatest(1, '5')", "5"),
        ("select case when true then 1 else '7' end", "1"),
        ("select 1 in (2, '1')", "t"),
        ("select pg_typeof(coalesce(1, 1.5))", "numeric"),
        ("select pg_typeof(array[1, 2.5])", "numeric[]"),
        ("select pg_typeof(case when true then 1 else 2.5::float8 end)", "double precision"),
        ("select pg_typeof(coalesce(1::int2, 2::int8))", "bigint"),
        ("select pg_typeof(coalesce(current_date, now()))", "timestamp with time zone"),
        ("select pg_typeof(array[1::int2, 2::int8])", "bigint[]"),
        ("select pg_typeof(greatest(1::float4, 2.5))", "real"),
        ("select pg_typeof(coalesce('a', 'b'))", "text"),
        ("select pg_typeof(coalesce(null, null))", "text"),
        ("select least(2, 1.25::float8, 3::int8)", "1.25"),
        ("select coalesce(sum(b), 0) from w", "2"),
        ("select coalesce(sum(b), 0.5) from w", "2"),
        ("select string_agg(coalesce(d, 0)::text, ',' order by a) from w", "1.25,0"),
        ("select string_agg((a in (1, 2.5))::text, ',' order by a) from w", "true,false"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    for (sql, code, message, position) in [
        ("select coalesce(1, 'a')", "22P02", "invalid input syntax for type integer: \"a\"", "20"),
        ("select array[1, 'a']", "22P02", "invalid input syntax for type integer: \"a\"", "17"),
        ("select array['a', 1]", "22P02", "invalid input syntax for type integer: \"a\"", "14"),
        ("select greatest(1, 'x')", "22P02", "invalid input syntax for type integer: \"x\"", "20"),
        ("select nullif(1, 'x')", "22P02", "invalid input syntax for type integer: \"x\"", "18"),
        (
            "select case when true then 1 else 'x' end",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
            "35",
        ),
        ("select 1 in (2, 'x')", "22P02", "invalid input syntax for type integer: \"x\"", "17"),
        (
            "select coalesce(now(), 'x')",
            "22007",
            "invalid input syntax for type timestamp with time zone: \"x\"",
            "24",
        ),
        (
            "select coalesce(1, 'a'::text)",
            "42804",
            "COALESCE types integer and text cannot be matched",
            "20",
        ),
        (
            "select array[1, 'a'::text]",
            "42804",
            "ARRAY types integer and text cannot be matched",
            "17",
        ),
        (
            "select coalesce(true, 1)",
            "42804",
            "COALESCE types boolean and integer cannot be matched",
            "23",
        ),
        (
            "select coalesce(1, 'a'::varchar(3))",
            "42804",
            "COALESCE types integer and character varying cannot be matched",
            "20",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(position), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_cast_between_timestamptz_and_a_type_without_a_zone_reads_the_time_zone_setting() {
    let dirs = Dirs::new("session-zone");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values of the PostgreSQL 19 oracle. The zone of the server process is not read, so the
    // first cast is midnight UTC on any machine.
    for (sql, value) in [
        ("select '2024-01-01'::date::timestamptz", "2024-01-01 00:00:00+00"),
        ("set timezone = 'America/New_York'", ""),
        ("select '2024-01-01'::date::timestamptz", "2024-01-01 00:00:00-05"),
        ("select '2024-07-01 23:30+00'::timestamptz::date", "2024-07-01"),
        ("select '2024-07-01 23:30+00'::timestamptz::timestamp", "2024-07-01 19:30:00"),
        ("select '2024-07-01 12:00'::timestamp::timestamptz", "2024-07-01 12:00:00-04"),
        ("select '2024-07-01 12:00+00'::timestamptz::text", "2024-07-01 08:00:00-04"),
        ("set time zone -3", ""),
        ("select '2024-01-01'::date::timestamptz", "2024-01-01 00:00:00-03"),
        ("select '2024-07-01 01:30+00'::timestamptz::date", "2024-06-30"),
        ("set time zone interval '+05:30'", ""),
        ("select '2024-01-01'::date::timestamptz", "2024-01-01 00:00:00+05:30"),
        ("set time zone interval '-02:30'", ""),
        (
            "select date_trunc('day', '2024-07-01 01:00+00'::timestamptz)",
            "2024-06-30 00:00:00-02:30",
        ),
        ("reset timezone", ""),
        ("select '2024-01-01'::date::timestamptz", "2024-01-01 00:00:00+00"),
    ] {
        if value.is_empty() {
            assert_eq!(tags(&client.query(sql)), "CSZ", "{sql}");
        } else {
            assert_eq!(scalar(&mut client, sql), value, "{sql}");
        }
    }
    server.stop().unwrap();
}

#[test]
fn to_char_to_timestamp_and_to_date_follow_the_templates_of_postgresql() {
    let dirs = Dirs::new("formatting");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values of the PostgreSQL 19 oracle.
    for (sql, value) in [
        (
            "select to_char(timestamp '2024-03-10 13:04:06', 'FMDay, FMMonth DDth YYYY HH12:MI PM')",
            "Sunday, March 10th 2024 01:04 PM",
        ),
        ("select to_char(date '2024-03-10', 'YYYY-MM-DD HH24:MI TZ')", "2024-03-10 00:00 UTC"),
        ("select to_char(time '13:04:05.5', 'HH24:MI:SS.MS HH12 AM')", "13:04:05.500 01 PM"),
        ("select to_char(interval '100 hours', 'HH24 HH12 SSSS')", "100 04 360000"),
        ("select to_timestamp('2024 070', 'YYYY DDD')", "2024-03-10 00:00:00+00"),
        ("select to_date('20240701', 'YYYYMMDD')", "2024-07-01"),
        ("set timezone = 'America/New_York'", ""),
        ("select to_char(timestamptz '2024-03-10 07:00:00+00', 'HH24:MI TZ OF')", "03:00 EDT -04"),
        ("select to_timestamp('2024-07-01 12:00', 'YYYY-MM-DD HH24:MI')", "2024-07-01 12:00:00-04"),
        (
            "select to_timestamp('2024-07-01 12:00 EST', 'YYYY-MM-DD HH24:MI TZ')",
            "2024-07-01 13:00:00-04",
        ),
        ("reset timezone", ""),
        ("select to_char(d, f) from (values (date '2024-03-04', 'DD/MM')) v(d, f)", "04/03"),
    ] {
        if value.is_empty() {
            assert_eq!(tags(&client.query(sql)), "CSZ", "{sql}");
        } else {
            assert_eq!(scalar(&mut client, sql), value, "{sql}");
        }
    }
    let messages = client.query("select to_char(timestamp 'infinity', 'YYYY') is null");
    assert_eq!(data_row(&messages[1]), [Some(b"t".to_vec())]);
    let messages = client.query("select to_timestamp('2024-07-01 1', 'YYYY-MM-DD HH24MI')");
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("22007"));
    let message = error.field(b'M');
    assert_eq!(message.as_deref(), Some("source string too short for \"HH24\" formatting field"));
    let messages = client.query("select to_char(interval '1 day', 'Day')");
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("22007"));
    assert_eq!(
        error.field(b'H').as_deref(),
        Some("Intervals are not tied to specific calendar dates.")
    );
    server.stop().unwrap();
}

#[test]
fn to_char_of_a_number_and_to_number_follow_the_templates_of_postgresql() {
    let dirs = Dirs::new("numformat");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values of the PostgreSQL 19 oracle.
    for (sql, value) in [
        ("select to_char(-1234567, 'FMS9,999,999')", "-1,234,567"),
        ("select to_char(12345678901::int8, '99999999999th')", " 12345678901st"),
        ("select to_char(-12.5::numeric, '999.99PR')", " <12.50>"),
        ("select to_char(1234.5::float8, '9.99EEEE')", " 1.23e+03"),
        ("select to_char(1.23456::float4, '9.999999')", " 1.23456"),
        ("select to_char(123::int2, '999')", " 123"),
        ("select to_char(485, 'FMRN')", "CDLXXXV"),
        ("select to_char(a, '9,999.99') from (values (1234.5::numeric(10, 2))) v(a)", " 1,234.50"),
        ("select to_number('<12.5>', '99.9PR')", "-12.5"),
        ("select to_number('12345', '999V99')", "123.450000000000000000"),
        ("select pg_typeof(to_number('12', '99'))", "numeric"),
        ("select to_number(s, f) from (values ('1,234', '9G999')) v(s, f)", "1234"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    let messages = client.query("select to_number('12', '99.99.9')");
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("42601"));
    assert_eq!(error.field(b'M').as_deref(), Some("multiple decimal points"));
    let messages = client.query("select to_number('IIII', 'RN')");
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("22P02"));
    server.stop().unwrap();
}

#[test]
fn pg_input_is_valid_and_pg_input_error_info_catch_the_error_of_the_input() {
    let dirs = Dirs::new("pginput");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values of the PostgreSQL 19 oracle.
    for (sql, value) in [
        ("select pg_input_is_valid('12', 'int4')", "t"),
        ("select pg_input_is_valid('70000', 'int2')", "f"),
        (
            "select pg_input_error_info('x', 'int4')",
            "(\"invalid input syntax for type integer: \"\"x\"\"\",,,22P02)",
        ),
        ("select pg_input_error_info('12', 'int4')", "(,,,)"),
        ("select (pg_input_error_info('70000', 'int2')).sql_error_code", "22003"),
        ("select pg_typeof(pg_input_error_info('x', 'int4'))", "record"),
        (
            "select pg_input_error_info('{\"a\":', 'json')",
            "(\"invalid input syntax for type json\",\"The input string ended unexpectedly.\",,22P02)",
        ),
        (
            "select pg_input_error_info('abcd', 'varchar(3)')",
            "(\"value too long for type character varying(3)\",,,22001)",
        ),
        (
            "select pg_input_error_info('(1)', 'record')",
            "(\"input of anonymous composite types is not implemented\",,,0A000)",
        ),
        (
            "select pg_input_error_info('abc', 'regtype')",
            "(\"type \"\"abc\"\" does not exist\",,,42704)",
        ),
        ("select pg_input_is_valid('pg_catalog.int4[]', 'regtype')", "t"),
        ("select pg_input_is_valid('13/01/2024', 'date')", "f"),
        (
            "select string_agg(pg_input_is_valid(x, 'int4')::text, ',') from (values ('1'), ('a')) v(x)",
            "true,false",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    client.query("set datestyle = 'ISO, DMY'");
    assert_eq!(scalar(&mut client, "select pg_input_is_valid('13/01/2024', 'date')"), "t");
    // An error of the name of the type is not caught.
    for (sql, code, message, context) in [
        (
            "select pg_input_is_valid('1', 'nosuch')",
            "42704",
            "type \"nosuch\" does not exist",
            None,
        ),
        (
            "select pg_input_is_valid('1', 'int4(')",
            "42601",
            "syntax error at end of input",
            Some("invalid type name \"int4(\""),
        ),
        (
            "select pg_input_is_valid('1', 'int4(3)')",
            "42601",
            "type modifier is not allowed for type \"int4\"",
            None,
        ),
        (
            "select pg_input_is_valid('1', 'numeric(1001)')",
            "22023",
            "NUMERIC precision 1001 must be between 1 and 1000",
            None,
        ),
    ] {
        let messages = client.query(sql);
        let error = messages.iter().find(|m| m.tag == b'E').unwrap();
        assert_eq!(error.field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'W').as_deref(), context, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_function_in_from_is_a_relation_and_a_row_has_the_fields_f1_to_fn() {
    let dirs = Dirs::new("fromfn");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        let shape = messages.iter().find(|m| m.tag == b'T').unwrap();
        let names: Vec<String> = row_shape(shape).into_iter().map(|(name, ..)| name).collect();
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(String::new, |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        (names.join(","), rows.join(";"))
    };
    // The values and the names of the columns of the PostgreSQL 19 oracle.
    for (sql, names, rows) in [
        ("select * from upper('abc')", "upper", "ABC"),
        ("select * from upper('abc') u", "u", "ABC"),
        ("select * from upper('abc') u(x)", "x", "ABC"),
        ("select u.* from abs(-3) a, lower('X') u", "u", "x"),
        (
            "select * from pg_input_error_info('x', 'int4')",
            "message,detail,hint,sql_error_code",
            "invalid input syntax for type integer: \"x\"|||22P02",
        ),
        ("select * from regexp_split_to_table('a,b,c', ',') t(x) where x <> 'b'", "x", "a;c"),
        ("select * from (values (1), (2)) v(n), lateral upper('x' || n) u", "n,u", "1|X1;2|X2"),
        ("select (row(1, 'a'::text)).f2, (r).f1 from (select row(2, 'b') r) s", "f2,f1", "a|2"),
    ] {
        assert_eq!(result(sql), (names.to_string(), rows.to_string()), "{sql}");
    }
    for (sql, code, message, position) in [
        (
            "select (row(1, 'a')).f3",
            "42703",
            "could not identify column \"f3\" in record data type",
            Some("9"),
        ),
        (
            "select * from upper('a') u(a, b)",
            "42P10",
            "table \"u\" has 1 columns available but 2 columns specified",
            None,
        ),
    ] {
        let messages = client.query(sql);
        let error = messages.iter().find(|m| m.tag == b'E').unwrap();
        assert_eq!(error.field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(error.field(b'P').as_deref(), position, "{sql}");
    }
    server.stop().unwrap();
}

/// `WITH ORDINALITY` numbers the rows of a function in `FROM` from 1, in a BIGINT column named
/// `ordinality` that a column list can rename. A lateral call numbers the rows of each outer row.
#[test]
fn with_ordinality_numbers_the_rows_of_a_function_in_from() {
    let dirs = Dirs::new("ordinal");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        let shape = messages.iter().find(|m| m.tag == b'T').unwrap();
        let names: Vec<String> = row_shape(shape).into_iter().map(|(name, ..)| name).collect();
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(String::new, |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        (names.join(","), rows.join(";"))
    };
    // The values and the names of the columns of the PostgreSQL 19 oracle.
    for (sql, names, rows) in [
        (
            "select * from generate_series(1, 2) with ordinality",
            "generate_series,ordinality",
            "1|1;2|2",
        ),
        ("select * from generate_series(1, 2) with ordinality g", "g,ordinality", "1|1;2|2"),
        ("select * from generate_series(1, 2) with ordinality as t(a)", "a,ordinality", "1|1;2|2"),
        (
            "select * from unnest(array['a', 'b']) with ordinality as u(x, i) where i > 1",
            "x,i",
            "b|2",
        ),
        ("select * from repeat('ab', 2) with ordinality", "repeat,ordinality", "abab|1"),
        (
            "select * from regexp_split_to_table('a,b', ',') with ordinality r",
            "r,ordinality",
            "a|1;b|2",
        ),
        (
            "select * from pg_input_error_info('x', 'int4') with ordinality",
            "message,detail,hint,sql_error_code,ordinality",
            "invalid input syntax for type integer: \"x\"|||22P02|1",
        ),
        (
            "select * from (values (2), (3)) v(n), lateral generate_series(1, v.n) with ordinality g(x, i) order by 1, 3",
            "n,x,i",
            "2|1|1;2|2|2;3|1|1;3|2|2;3|3|3",
        ),
        (
            "select pg_typeof(g), pg_typeof(ordinality) from generate_series(1, 1) with ordinality g",
            "pg_typeof,pg_typeof",
            "integer|bigint",
        ),
    ] {
        assert_eq!(result(sql), (names.to_string(), rows.to_string()), "{sql}");
    }
    let messages = client.query("select * from upper('x') with ordinality as u(a, b, c)");
    let error = messages.iter().find(|m| m.tag == b'E').unwrap();
    assert_eq!(error.field(b'C').as_deref(), Some("42P10"));
    assert_eq!(
        error.field(b'M').as_deref(),
        Some("table \"u\" has 2 columns available but 3 columns specified")
    );
    server.stop().unwrap();
}

/// `ROWS FROM` puts the rows of its calls side by side, and a call with fewer rows gives nulls. A
/// call that does not return a set gives one row. `unnest` of more than one array is the
/// `ROWS FROM` of an `unnest` for each array, and a LATERAL call is made for each outer row.
#[test]
fn rows_from_puts_the_rows_of_its_calls_side_by_side() {
    let dirs = Dirs::new("rowsfrom");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        let shape = messages.iter().find(|m| m.tag == b'T').unwrap();
        let names: Vec<String> = row_shape(shape).into_iter().map(|(name, ..)| name).collect();
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(String::new, |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        (names.join(","), rows.join(";"))
    };
    // The values and the names of the columns of the PostgreSQL 19 oracle.
    for (sql, names, rows) in [
        (
            "select * from rows from (generate_series(1, 2), generate_series(1, 3))",
            "generate_series,generate_series",
            "1|1;2|2;|3",
        ),
        (
            "select * from rows from (generate_series(1, 2), generate_series(1, 3)) with ordinality as t(a, b, n)",
            "a,b,n",
            "1|1|1;2|2|2;|3|3",
        ),
        (
            "select * from rows from (upper('a'), generate_series(1, 2))",
            "upper,generate_series",
            "A|1;|2",
        ),
        ("select * from unnest(array[1, 2], array['a', 'b', 'c'])", "unnest,unnest", "1|a;2|b;|c"),
        ("select * from unnest(array[1, 2], array['a']) as u(x)", "x,unnest", "1|a;2|"),
        ("select * from rows from (generate_series(1, 2)) r", "r", "1;2"),
        (
            "select * from rows from (pg_input_error_info('x', 'int4'), generate_series(1, 2))",
            "message,detail,hint,sql_error_code,generate_series",
            "invalid input syntax for type integer: \"x\"|||22P02|1;||||2",
        ),
        (
            "select * from (values (1), (2)) v(n), lateral rows from (generate_series(1, n), unnest(array['x'])) r",
            "n,generate_series,unnest",
            "1|1|x;2|1|x;2|2|",
        ),
        (
            "select * from (values (1), (2)) v(n), lateral (select generate_series(1, n)) s",
            "n,generate_series",
            "1|1;2|1;2|2",
        ),
        ("select pg_typeof(generate_series(1::int8, 1))", "pg_typeof", "bigint"),
    ] {
        assert_eq!(result(sql), (names.to_string(), rows.to_string()), "{sql}");
    }
    server.stop().unwrap();
}

/// `(r).*` is a column for each field of `r`, named by the field, in a select list and in a row.
/// Anywhere else it is refused.
#[test]
fn a_star_after_a_record_is_a_column_for_each_field() {
    let dirs = Dirs::new("fields");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let code = error.field(b'C').unwrap_or_default();
            let text = error.field(b'M').unwrap_or_default();
            return (format!("{code} {text}"), String::new());
        }
        let shape = messages.iter().find(|m| m.tag == b'T').unwrap();
        let names: Vec<String> = row_shape(shape).into_iter().map(|(name, ..)| name).collect();
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(String::new, |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        (names.join(","), rows.join(";"))
    };
    // The values and the names of the columns of the PostgreSQL 19 oracle.
    for (sql, names, rows) in [
        ("select (row(1, 'a'::text)).*", "f1,f2", "1|a"),
        (
            "select (pg_input_error_info('x', 'int4')).* as q",
            "message,detail,hint,sql_error_code",
            "invalid input syntax for type integer: \"x\"|||22P02",
        ),
        ("select (r).*, 9 as z from (select row(1, 'a'::text) r) s", "f1,f2,z", "1|a|9"),
        ("select (r).f1, ((r).*) from (select row(2, 3) r) s", "f1,f1,f2", "2|2|3"),
        ("select (row(row(1, 2), 3)).*", "f1,f2", "(1,2)|3"),
        ("select sum((r).f1), (r).* from (select row(1, 2) r) s group by r", "sum,f1,f2", "1|1|2"),
        ("select row((r).*, 3) from (select row(1, 2) r) s", "row", "(1,2,3)"),
        ("select (1).*", "42809 type integer is not composite", ""),
        ("select ('a'::text).*", "42809 type text is not composite", ""),
        (
            "select count((r).*) from (select row(1, 2) r) s",
            "0A000 row expansion via \"*\" is not supported here",
            "",
        ),
    ] {
        assert_eq!(result(sql), (names.to_string(), rows.to_string()), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_json_set_functions_give_the_rows_and_columns_of_postgresql() {
    let dirs = Dirs::new("json-sets");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let code = error.field(b'C').unwrap_or_default();
            let text = error.field(b'M').unwrap_or_default();
            return (format!("{code} {text}"), String::new());
        }
        let shape = messages.iter().find(|m| m.tag == b'T').unwrap();
        let names: Vec<String> = row_shape(shape).into_iter().map(|(name, ..)| name).collect();
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(String::new, |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        (names.join(","), rows.join(";"))
    };
    // The values and the names of the columns of the PostgreSQL 19 oracle. `json` keeps the text
    // and the duplicate keys of the document, and `jsonb` sorts the keys and keeps the last value.
    for (sql, names, rows) in [
        (
            r#"select * from json_each('{"a":1,"b":"x","c":null,"d":[1]}')"#,
            "key,value",
            r#"a|1;b|"x";c|null;d|[1]"#,
        ),
        (
            r#"select * from json_each_text('{"a":1,"b":"x","c":null,"d":[1]}')"#,
            "key,value",
            "a|1;b|x;c|;d|[1]",
        ),
        (
            r#"select * from json_each('{"a":{"x" :  1},"a":2}')"#,
            "key,value",
            r#"a|{"x" :  1};a|2"#,
        ),
        (
            r#"select * from jsonb_each('{"b":1,"a":{"x" :  1}, "b":3}')"#,
            "key,value",
            r#"a|{"x": 1};b|3"#,
        ),
        (r#"select * from json_each_text('{"a":"é\n"}')"#, "key,value", "a|\u{e9}\n"),
        (r#"select * from json_each('{"a":1}') as t(k, v)"#, "k,v", "a|1"),
        (r#"select * from json_each('{"a":1}') with ordinality"#, "key,value,ordinality", "a|1|1"),
        ("select * from json_each(null)", "key,value", ""),
        (r#"select * from json_array_elements('[1,"a",null]') e"#, "value", r#"1;"a";null"#),
        (r#"select * from json_array_elements_text('[1,"a\"b",null]')"#, "value", r#"1;a"b;"#),
        ("select * from json_array_elements('[1]') as e(x)", "x", "1"),
        (
            r#"select * from jsonb_array_elements('[{"b":1,"a":2}]')"#,
            "value",
            r#"{"a": 2, "b": 1}"#,
        ),
        (r#"select * from json_object_keys('{"a":1,"b":2}')"#, "json_object_keys", "a;b"),
        (r#"select * from json_object_keys('{"a":1}') k"#, "k", "a"),
        (r#"select * from jsonb_object_keys('{"b":1,"a":2}')"#, "jsonb_object_keys", "a;b"),
        (r#"select pg_typeof(value) from jsonb_each('{"a":1}')"#, "pg_typeof", "jsonb"),
        (r#"select pg_typeof(value) from json_each_text('{"a":1}')"#, "pg_typeof", "text"),
        (r#"select json_each('{"a":1,"b":[2]}')"#, "json_each", "(a,1);(b,[2])"),
        (r#"select (json_each('{"a":1}')).key"#, "key", "a"),
        (r#"select json_object_keys('{"a":1}') || 'x'"#, "?column?", "ax"),
        (
            "select generate_series(1, 2), json_array_elements_text('[1,2,3]')",
            "generate_series,json_array_elements_text",
            "1|1;2|2;|3",
        ),
        (
            r#"select * from (values ('{"a":1}'::json), ('{"b":2,"c":3}')) v(j), json_each(v.j)"#,
            "j,key,value",
            r#"{"a":1}|a|1;{"b":2,"c":3}|b|2;{"b":2,"c":3}|c|3"#,
        ),
        ("select * from json_each('[1]')", "22023 cannot deconstruct an array as an object", ""),
        ("select * from json_each('1')", "22023 cannot deconstruct a scalar", ""),
        (
            r#"select * from json_array_elements('{"a":1}')"#,
            "22023 cannot call json_array_elements on a non-array",
            "",
        ),
        (
            "select * from json_object_keys('[1]')",
            "22023 cannot call json_object_keys on an array",
            "",
        ),
        (
            r#"select * from json_each('{"a":1}'::jsonb)"#,
            "42883 function json_each(jsonb) does not exist",
            "",
        ),
        ("select json_each(1)", "42883 function json_each(integer) does not exist", ""),
        (
            r#"select 1 where json_object_keys('{"a":1}') = 'a'"#,
            "0A000 set-returning functions are not allowed in WHERE",
            "",
        ),
        (
            "select 1 where generate_series(1, 2) = 1",
            "0A000 set-returning functions are not allowed in WHERE",
            "",
        ),
        (
            r#"select count(json_each('{"a":1}'))"#,
            "0A000 aggregate function calls cannot contain set-returning function calls",
            "",
        ),
        (
            "select 1 limit json_array_length(json_array_elements('[1]'))",
            "0A000 set-returning functions are not allowed in LIMIT",
            "",
        ),
    ] {
        assert_eq!(result(sql), (names.to_string(), rows.to_string()), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_regular_expressions_of_postgresql_give_its_matches_and_its_rows() {
    let dirs = Dirs::new("pg-regexp");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let code = error.field(b'C').unwrap_or_default();
            let text = error.field(b'M').unwrap_or_default();
            return (format!("{code} {text}"), String::new());
        }
        let shape = messages.iter().find(|m| m.tag == b'T').unwrap();
        let names: Vec<String> = row_shape(shape).into_iter().map(|(name, ..)| name).collect();
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(String::new, |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        (names.join(","), rows.join(";"))
    };
    // The values, the names and the errors of the PostgreSQL 19 oracle. The match is the longest
    // from the leftmost start, and `regexp_matches` gives a row for each match.
    for (sql, names, rows) in [
        ("select regexp_match('abcd', '(a|ab)(c|bcd)(d*)')", "regexp_match", "{ab,c,d}"),
        ("select regexp_match('abc', 'x')", "regexp_match", ""),
        ("select (regexp_match('abc', '(b)(c)'))[2]", "regexp_match", "c"),
        ("select pg_typeof(regexp_match('abc', 'b'))", "pg_typeof", "text[]"),
        ("select regexp_matches('aBab', 'b', 'gi')", "regexp_matches", "{B};{b}"),
        ("select regexp_matches('abc', 'x*', 'g')", "regexp_matches", r#"{""};{""};{""};{""}"#),
        ("select * from regexp_matches('ab', '(b)(x)?') m", "m", "{b,NULL}"),
        ("select * from regexp_matches('ab', 'x')", "regexp_matches", ""),
        (
            "select x, m from (values ('ab'), ('c')) t(x), regexp_matches(x, '.', 'g') m",
            "x,m",
            "ab|{a};ab|{b};c|{c}",
        ),
        (
            "select * from regexp_matches('ab', '.', 'g') with ordinality",
            "regexp_matches,ordinality",
            "{a}|1;{b}|2",
        ),
        (
            "select 'abc' ~ 'B', 'abc' ~* 'B', 'abc' !~ 'b', 'ab' ~ '\\mb'",
            "?column?,?column?,?column?,?column?",
            "f|t|f|f",
        ),
        ("select x from (values ('abc'), ('xyz'), (null)) t(x) where x ~ '^x'", "x", "xyz"),
        (
            "select regexp_matches('ab', 'b', 'z')",
            "22023 invalid regular expression option: \"z\"",
            "",
        ),
        (
            "select regexp_match('abc', 'b', 'g')",
            "22023 regexp_match() does not support the \"global\" option",
            "",
        ),
        ("select 'abc' ~ '('", "2201B invalid regular expression: parentheses () not balanced", ""),
        (
            "select regexp_match(123, '1')",
            "42883 function regexp_match(integer, unknown) does not exist",
            "",
        ),
        ("select 1 ~ '1'", "42883 operator does not exist: integer ~ unknown", ""),
        (
            "select 1 where regexp_matches('ab', 'a') is not null",
            "0A000 set-returning functions are not allowed in WHERE",
            "",
        ),
        (
            "select regexp_count('abcabc', 'b'), regexp_instr('abcabc', 'c'), \
             regexp_like('ABC', 'b', 'i'), regexp_substr('abcabc', '(b)(c)', 1, 2, '', 2)",
            "regexp_count,regexp_instr,regexp_like,regexp_substr",
            "2|3|t|c",
        ),
        (
            "select regexp_count('abc', '', 4), regexp_count('abc', 'b', 5), \
             regexp_instr('héllo wörld', '[éö]', 3, 1, 1), regexp_instr('abc', 'b', '2')",
            "regexp_count,regexp_count,regexp_instr,regexp_instr",
            "1|0|9|2",
        ),
        (
            "select regexp_instr('abc', '(x)?b', 1, 1, 0, '', 1), \
             regexp_substr('abc', '(x)?b', 1, 1, '', 1), regexp_instr('abc', 'b', null)",
            "regexp_instr,regexp_substr,regexp_instr",
            "0||",
        ),
        (
            "select regexp_instr('abc', '(', 0)",
            "22023 invalid value for parameter \"start\": 0",
            "",
        ),
        (
            "select regexp_substr('abc', 'b', 1, 1, 'z', -1)",
            "22023 invalid value for parameter \"subexpr\": -1",
            "",
        ),
        (
            "select regexp_count('abc', 'b', 1, 'g')",
            "22023 regexp_count() does not support the \"global\" option",
            "",
        ),
        (
            "select regexp_instr('abc', 'b', 2::bigint)",
            "42883 function regexp_instr(unknown, unknown, bigint) does not exist",
            "",
        ),
        (
            "select regexp_replace('abcabc', 'b', 'X'), regexp_replace('abcabc', 'b', 'X', 'g'), \
             regexp_replace('abcabcabc', 'B', 'X', 1, 2, 'gi'), regexp_replace('abc', 'x*', '-', 'g')",
            "regexp_replace,regexp_replace,regexp_replace,regexp_replace",
            "aXcabc|aXcaXc|abcaXcabc|-a-b-c-",
        ),
        (
            "select regexp_replace('abcabc', '(b)(c)', '[\\2\\1\\&\\\\\\3\\x]', 'g'), \
             regexp_replace('abcabcabc', 'b', 'X', 1, 0), regexp_replace('abcabc', 'b', 'X', 7)",
            "regexp_replace,regexp_replace,regexp_replace",
            "a[cbbc\\\\x]a[cbbc\\\\x]|aXcaXcaXc|abcabc",
        ),
        (
            "select regexp_replace('abc', 'b', 'X', '2')",
            "22023 invalid regular expression option: \"2\"",
            "",
        ),
        (
            "select regexp_replace('abc', 'b', 'X', 1, -1)",
            "22023 invalid value for parameter \"n\": -1",
            "",
        ),
        (
            "select regexp_split_to_array('a,b,,c', ','), regexp_split_to_array('abc', 'x*'), \
             regexp_split_to_array(',a,', ','), regexp_split_to_array('', ',')",
            "regexp_split_to_array,regexp_split_to_array,regexp_split_to_array,\
             regexp_split_to_array",
            r#"{a,b,"",c}|{a,b,c}|{"",a,""}|{""}"#,
        ),
        (
            "select * from regexp_split_to_table('a b', '\\s+') with ordinality",
            "regexp_split_to_table,ordinality",
            "a|1;b|2",
        ),
        (
            "select regexp_split_to_table(x, ',') from (values ('a,b'), ('c')) t(x)",
            "regexp_split_to_table",
            "a;b;c",
        ),
        (
            "select regexp_split_to_array('abc', 'b', 'g')",
            "22023 regexp_split_to_array() does not support the \"global\" option",
            "",
        ),
        (
            "select substring('abcdef' from 'c.e'), substring('abcdef' from 'c(.)e'), \
             substring('abcdef' from '(x)?c'), substring('abcdef' from 2 for 3)",
            "substring,substring,substring,substring",
            "cde|d||bcd",
        ),
        (
            "select 'abc' similar to 'a%', 'a|c' similar to 'a|c', 'ab' similar to '(a|b)*', \
             'a%' similar to 'a#%' escape '#', 'abc' not similar to 'a_c', \
             'abc' similar to 'a%' escape null",
            "?column?,?column?,?column?,?column?,?column?,?column?",
            "t|f|t|t|f|",
        ),
        (
            "select similar_to_escape('a%b_c'), similar_to_escape('[^]a]'), \
             similar_to_escape('a#\"b#\"c', '#'), similar_to_escape('a\\%', '')",
            "similar_to_escape,similar_to_escape,similar_to_escape,similar_to_escape",
            "^(?:a.*b.c)$|^(?:[^]a])$|^(?:a){1,1}?(b){1,1}(?:c)$|^(?:a\\\\.*)$",
        ),
        (
            "select substring('foobar' similar '%#\"o_b#\"%' escape '#'), \
             substring('foobar' from '%#\"o%' for '#'), \
             substring('foobar' similar '#\"o_b#\"%' escape '#')",
            "substring,substring,substring",
            "oob|oobar|",
        ),
        (
            "select x from (values ('abc'), ('xbc'), (null)) t(x) where x similar to '_b%'",
            "x",
            "abc;xbc",
        ),
        ("select 'abc' similar to 'a%' escape 'xy'", "22025 invalid escape string", ""),
        (
            "select similar_to_escape('a#\"b#\"c#\"d', '#')",
            "2200C SQL regular expression may not contain more than two escape-double-quote \
             separators",
            "",
        ),
    ] {
        assert_eq!(result(sql), (names.to_string(), rows.to_string()), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn substring_from_a_position_keeps_the_part_that_postgresql_keeps() {
    let dirs = Dirs::new("pg-substring");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let code = error.field(b'C').unwrap_or_default();
            let text = error.field(b'M').unwrap_or_default();
            return format!("{code} {text}");
        }
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(|| "NULL".into(), |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        rows.join(";")
    };
    // The values and the errors of the PostgreSQL 19 oracle. The part starts at the start and
    // ends one before the start plus the length, and a start before the first character does not
    // count back from the end.
    for (sql, expected) in [
        ("select substring('abcdef', -1, 3)", "a"),
        ("select substring('abcdef', -1)", "abcdef"),
        ("select substring('abcdef', 0, 2)", "a"),
        ("select substring('abcdef', -5, 3)", ""),
        ("select substring('abcdef', 3, 100)", "cdef"),
        ("select substring('abcdef', 10)", ""),
        ("select substring('abcdef', -2147483647, 2147483647)", ""),
        ("select substring('abcdef', 1, 2147483647)", "abcdef"),
        ("select substring('héllo', 2, 2)", "él"),
        ("select substring('abcdef', 2, -1)", "22011 negative substring length not allowed"),
        ("select substr('abcdef', -1, 3)", "a"),
        ("select substring('abc' from 2 for '1')", "b"),
        ("select substring('abc' from '2' for 1)", "b"),
        ("select substring('abc' from '2')", "NULL"),
        ("select substr('abc', ' 2 ')", "bc"),
        ("select substr('abc', 'x')", "22P02 invalid input syntax for type integer: \"x\""),
        ("select regexp_instr('abc', 'b', '2')", "2"),
        ("select substring('abc' from 1::smallint)", "abc"),
        ("select substring('abc', null)", "NULL"),
        ("select substring('abc', 1, null)", "NULL"),
        (
            "select substr('abc', 1::bigint)",
            "42883 function substr(unknown, bigint) does not exist",
        ),
        (
            "select substring('abc', 1.5)",
            "42883 function substring(unknown, numeric) does not exist",
        ),
        ("select substring(123, 1)", "42883 function substring(integer, integer) does not exist"),
        ("select substring('\\x010203'::bytea, 2, 1)", "\\x02"),
        ("select substring('\\x010203'::bytea, -1, 3)", "\\x01"),
        ("select substring('\\x010203'::bytea from 2)", "\\x0203"),
        (
            "select substring(x, -1, 3), substring(x, 0), substr(x, 2, 0) \
             from (values ('abcdef'), ('héllo'), (null)) t(x)",
            "a|abcdef|;h|héllo|;NULL|NULL|NULL",
        ),
        (
            "select substring(x, y, z) from (values ('abcdef', -1, 3), ('abcdef', 2, 2)) t(x, y, z)",
            "a;bc",
        ),
        ("select substring(x, 2, -1) from (values (null::text)) t(x)", "NULL"),
    ] {
        assert_eq!(result(sql), expected, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn array_fill_gives_the_array_and_the_errors_of_postgresql() {
    let dirs = Dirs::new("pg-array-fill");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let code = error.field(b'C').unwrap_or_default();
            let text = error.field(b'M').unwrap_or_default();
            return format!("{code} {text}");
        }
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(|| "NULL".into(), |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        rows.join(";")
    };
    // The values and the errors of the PostgreSQL 19 oracle, in the order that PostgreSQL checks
    // the arguments. An array of more than one dimension or with another lower bound is not a
    // list, and is the one error that PostgreSQL does not give.
    for (sql, expected) in [
        ("select array_fill(7, array[3])", "{7,7,7}"),
        ("select array_fill(null::int, array[2])", "{NULL,NULL}"),
        ("select array_fill(7, array[2], array[1])", "{7,7}"),
        ("select array_fill(7, array[0], array[5])", "{}"),
        ("select array_fill(7, array[]::int[])", "{}"),
        ("select array_fill(7, '{2}', '{1}')", "{7,7}"),
        ("select array_fill(7, array[2::smallint])", "{7,7}"),
        ("select array_fill(row(1, 'a'), array[2])", "{\"(1,a)\",\"(1,a)\"}"),
        ("select pg_typeof(array_fill(7, array[2]))", "integer[]"),
        (
            "select array_fill(x, array[2], array[y]) from (values (1, 1), (2, 1)) t(x, y)",
            "{1,1};{2,2}",
        ),
        ("select array_fill(7, null)", "22004 dimension array or low bound array cannot be null"),
        ("select array_fill(7, array[null, 2]::int[])", "22004 dimension values cannot be null"),
        (
            "select array_fill(7, array[1, 1, 1, 1, 1, 1, 1], array[1])",
            "54000 number of array dimensions (7) exceeds the maximum allowed (6)",
        ),
        (
            "select array_fill(7, array[1], array[]::int[])",
            "2202E wrong number of array subscripts",
        ),
        (
            "select array_fill(7, array[-1])",
            "54000 array size exceeds the maximum allowed (134217727)",
        ),
        (
            "select array_fill(7, array[2], array[2147483647])",
            "54000 array lower bound is too large: 2147483647",
        ),
        (
            "select array_fill('a', array[2])",
            "42804 could not determine polymorphic type because input has type unknown",
        ),
        (
            "select array_fill(array[1, 2], array[2])",
            "42704 could not find array type for data type integer[]",
        ),
        (
            "select array_fill(7, array[2::bigint])",
            "42883 function array_fill(integer, bigint[]) does not exist",
        ),
        (
            "select array_fill(7, array[2, 2])",
            "0A000 arrays of more than one dimension are not supported",
        ),
    ] {
        assert_eq!(result(sql), expected, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_table_name_is_the_whole_row_as_postgresql_has_it() {
    let dirs = Dirs::new("pg-whole-row");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let mut result = |sql: &str| {
        let messages = client.query(sql);
        if let Some(error) = messages.iter().find(|m| m.tag == b'E') {
            let code = error.field(b'C').unwrap_or_default();
            let text = error.field(b'M').unwrap_or_default();
            return format!("{code} {text}");
        }
        let rows: Vec<String> = messages
            .iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                let values = data_row(m).into_iter().map(|value| {
                    value.map_or_else(|| "NULL".into(), |value| String::from_utf8(value).unwrap())
                });
                values.collect::<Vec<_>>().join("|")
            })
            .collect();
        rows.join(";")
    };
    result("create table t(a int, b text)");
    result("insert into t values (1, 'x'), (2, null)");
    // The answers of the PostgreSQL 19 oracle. A row is null when each field is null and not null
    // when no field is, and `t.*` in a row constructor or a call is the row of `t`.
    for (sql, expected) in [
        ("select t from t order by a", "(1,x);(2,)"),
        ("select (t).*, (t).a from t order by a", "1|x|1;2|NULL|2"),
        ("select s from t s order by a", "(1,x);(2,)"),
        ("select t from t as x", "42703 column \"t\" does not exist"),
        ("select x from (select 1 as p, 'q' as r) x", "(1,q)"),
        ("select x from (values (1, 2)) x(c, d)", "(1,2)"),
        (
            "select t::text, row_to_json(t), to_json(t) from t where a = 1",
            "(1,x)|{\"a\":1,\"b\":\"x\"}|{\"a\":1,\"b\":\"x\"}",
        ),
        ("select t = t, count(t) over () from t order by a", "t|2;t|2"),
        ("select array_agg(t order by a) from t", "{\"(1,x)\",\"(2,)\"}"),
        ("select (select t from (values (9)) v(z)) from t order by a", "(1,x);(2,)"),
        ("select t from t, (values (1)) u(t) order by a", "1;1"),
        ("select u from t, (values (1)) u(t) order by a", "(1);(1)"),
        (
            "select t is null, t is not null, t isnull, t notnull from t order by a",
            "f|t|f|t;f|f|f|f",
        ),
        ("select t from t where t is not null", "(1,x)"),
        (
            "select row(1, null) is null, row(1, null) is not null, row(null, null) is null, \
             row(null, null) is not null, row(1, 2) is not null",
            "f|f|t|f|t",
        ),
        ("select row(row(null)) is null, row(row(null)) is not null", "f|t"),
        ("select row(t.*), row(t.*, 3), (1, t.*) from t where a = 1", "(1,x)|(1,x,3)|(1,1,x)"),
        (
            "select row_to_json(t.*), count(t.*) over () from t where a = 2",
            "{\"a\":2,\"b\":null}|1",
        ),
        (
            "select b is null, b is not null from (values (1)) a(x) left join (values (2)) b(y) on false",
            "t|f",
        ),
    ] {
        assert_eq!(result(sql), expected, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn the_columns_of_values_and_of_a_set_operation_take_the_common_type_of_postgresql() {
    let dirs = Dirs::new("set-op-type");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values and the errors of the PostgreSQL 19 oracle. A string literal that a `SELECT`
    // writes as a column is read with the input function of the type of the other side.
    for (sql, value) in [
        ("select string_agg(column1::text, ',') from (values (1), ('2')) v", "1,2"),
        ("select pg_typeof(column1) from (values (1), (2.5)) v limit 1", "numeric"),
        (
            "select pg_typeof(x) from (values (now()), ('2024-01-01')) v(x) limit 1",
            "timestamp with time zone",
        ),
        ("select pg_typeof(column1) from (values (null), (null)) v", "text"),
        ("select string_agg(x::text, ',' order by x) from (select 1 x union select '2') s", "1,2"),
        ("select pg_typeof(x) from (select 1 as x union all select 2.5) s limit 1", "numeric"),
        ("select pg_typeof(x) from (select null as x union select null) s", "text"),
        (
            "select string_agg(x::text, ',' order by x) from (select 1 x union select 2.5 union select '3') s",
            "1,2.5,3",
        ),
        ("select 1 intersect select '1'", "1"),
        ("select '1' intersect select 1::int2", "1"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    for (sql, code, message, position) in [
        ("values (1), ('x')", "22P02", "invalid input syntax for type integer: \"x\"", "14"),
        (
            "values (1, 'a'), ('2', 3)",
            "22P02",
            "invalid input syntax for type integer: \"a\"",
            "12",
        ),
        ("values (1), (true)", "42804", "VALUES types integer and boolean cannot be matched", "14"),
        (
            "values ('1'::varchar), (2)",
            "42804",
            "VALUES types character varying and integer cannot be matched",
            "25",
        ),
        (
            "select 1 union select 'x'",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
            "23",
        ),
        ("select 'a' union select 1", "22P02", "invalid input syntax for type integer: \"a\"", "8"),
        (
            "select 1, 'a' union select 2, 3",
            "22P02",
            "invalid input syntax for type integer: \"a\"",
            "11",
        ),
        (
            "select 1 except select '1.5'",
            "22P02",
            "invalid input syntax for type integer: \"1.5\"",
            "24",
        ),
        (
            "select 1 union select true",
            "42804",
            "UNION types integer and boolean cannot be matched",
            "23",
        ),
        (
            "select 1 union all select 'x'::text",
            "42804",
            "UNION types integer and text cannot be matched",
            "27",
        ),
        (
            "select 1 union (select true union select false)",
            "42804",
            "UNION types integer and boolean cannot be matched",
            "24",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(position), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_name_in_from_is_the_name_of_one_item_only() {
    let dirs = Dirs::new("table-names");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for sql in ["create temp table t (a int, b text)", "create temp table u (a int, d int)"] {
        assert_eq!(tags(&client.query(sql)), "CZ", "{sql}");
    }
    // The cases of the PostgreSQL 19 oracle. A name is the alias of an item, or the name of a
    // table, a function or a `WITH` that has no alias. Two tables with no alias are two names
    // only when they are two tables.
    for sql in [
        "select 1 from t, t x",
        "select 1 from t x join t y on true",
        "select 1 from (select 1), (select 2)",
    ] {
        assert!(!tags(&client.query(sql)).contains('E'), "{sql}");
    }
    for (sql, name) in [
        ("select * from t join u on true join t on true", "t"),
        ("with w as (select 1) select * from w, w", "w"),
        ("select 1 from t, t", "t"),
        ("select 1 from t x, u x", "x"),
        ("select 1 from (select 1) s, (select 2) s", "s"),
        ("select 1 from generate_series(1, 2), generate_series(1, 3)", "generate_series"),
        ("select 1 from generate_series(1, 2) g, t g", "g"),
        ("select 1 from t join t on nope", "t"),
        ("select 1 from t, pg_catalog.pg_class, pg_class", "pg_class"),
        ("select 1 from t natural join t", "t"),
        ("select 1 from (values (1)) v, (values (2)) v", "v"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42712"), "{sql}");
        let message = format!("table name \"{name}\" specified more than once");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message.as_str()), "{sql}");
        assert_eq!(messages[0].field(b'P'), None, "{sql}");
    }
    // The first name that is not found is the error, before the names are compared.
    let messages = client.query("select 1 from nope, nope");
    assert_eq!(messages[0].field(b'C').as_deref(), Some("42P01"));
    server.stop().unwrap();
}

#[test]
fn distinct_sorts_on_what_it_selects_and_a_count_is_a_bigint() {
    let dirs = Dirs::new("distinct-order");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for sql in [
        "create temp table t (a int, b text, c numeric)",
        "insert into t values (1, 'x', 1), (2, 'y', 2), (3, 'z', 3)",
    ] {
        assert_eq!(tags(&client.query(sql)), "CZ", "{sql}");
    }
    // The values and the errors of the PostgreSQL 19 oracle.
    for (sql, value) in [
        (
            "select string_agg(x::text, ',') from (select distinct a as x from t order by a) s",
            "1,2,3",
        ),
        (
            "select string_agg(x::text, ',') from (select distinct a + 1 x from t order by a + 1) s",
            "2,3,4",
        ),
        (
            "select string_agg(b, ',') from (select distinct on (a, b) a, b from t order by b, a, c) s",
            "x,y,z",
        ),
        (
            "select string_agg(b, ',') from (select distinct on (a) a, b from t order by a, b, a) s",
            "x,y,z",
        ),
        ("select count(*) from (select a from t limit '2') s", "2"),
        ("select count(*) from (select a from t limit 2.5) s", "3"),
        ("select count(*) from (select a from t limit 2.5::float8) s", "2"),
        ("select count(*) from (select a from t limit 2::int2) s", "2"),
        ("select count(*) from (select a from t limit (select 1)) s", "1"),
    ] {
        assert_eq!(scalar(&mut client, sql), value, "{sql}");
    }
    let distinct = "for SELECT DISTINCT, ORDER BY expressions must appear in select list";
    let on = "SELECT DISTINCT ON expressions must match initial ORDER BY expressions";
    for (sql, code, message, position) in [
        ("select distinct a from t order by b", "42P10", distinct, "35"),
        ("select distinct a from t order by a + 1", "42P10", distinct, "35"),
        ("select distinct on (a) a, b from t order by b", "42P10", on, "21"),
        ("select distinct on (a, b) a, b from t order by b, c, a", "42P10", on, "21"),
        ("select distinct on (a, b) a, b from t order by a, c", "42P10", on, "24"),
        ("select distinct on (b, a) a, b from t order by a, c", "42P10", on, "21"),
        ("select a from t limit 'x'", "22P02", "invalid input syntax for type bigint: \"x\"", "23"),
        (
            "select a from t offset 'x'",
            "22P02",
            "invalid input syntax for type bigint: \"x\"",
            "24",
        ),
        (
            "select a from t limit true",
            "42804",
            "argument of LIMIT must be type bigint, not type boolean",
            "23",
        ),
        (
            "select a from t offset true",
            "42804",
            "argument of OFFSET must be type bigint, not type boolean",
            "24",
        ),
        (
            "select a from t limit '2'::text",
            "42804",
            "argument of LIMIT must be type bigint, not type text",
            "23",
        ),
        (
            "select a from t limit (select true)",
            "42804",
            "argument of LIMIT must be type bigint, not type boolean",
            "23",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(position), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_table_or_a_view_of_a_query_has_no_two_columns_of_one_name() {
    let dirs = Dirs::new("query-columns");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The cases of the PostgreSQL 19 oracle. A column list renames the first columns before the
    // names are compared, and a quoted name is compared as written.
    for sql in [
        "create temp table t3 (z) as select 1 as x, 2 as x",
        "create temp view v2 (z) as select 1 as x, 2 as x",
        "create temp view v5 as select 1 as \"X\", 2 as x",
        "create temp table t5 as select 1 as \"X\", 2 as x",
    ] {
        assert!(!tags(&client.query(sql)).contains('E'), "{sql}");
    }
    for (sql, name) in [
        ("create temp table t2 as select 1 as x, 2 as x", "x"),
        ("create temp table t4 (x) as select 1 as y, 2 as x", "x"),
        ("create temp table t6 as select 1, 2", "?column?"),
        ("create temp table t7 as select 1 as a, 2 as A", "a"),
        ("create temp view v1 as select 1 as x, 2 as x", "x"),
        ("create temp view v3 (a, a) as select 1, 2", "a"),
        ("create temp view v4 (x) as select 1 as y, 2 as x", "x"),
        ("create temp view v6 as select 1, 2", "?column?"),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42701"), "{sql}");
        let message = format!("column \"{name}\" specified more than once");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message.as_str()), "{sql}");
        assert_eq!(messages[0].field(b'P'), None, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_written_column_is_placed_at_its_name() {
    let dirs = Dirs::new("column-positions");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    assert!(!tags(&client.query("create temp table t (a int, b text, c numeric)")).contains('E'));
    // The cases of the PostgreSQL 19 oracle. An error about a column of the INSERT list or of the
    // SET list is placed at the name of that column, and a repeated SET column has no place.
    for (sql, state, message, position) in [
        (
            "insert into t (a, a) values (1, 2)",
            "42701",
            "column \"a\" specified more than once",
            Some("19"),
        ),
        (
            "insert into t (z) values (1)",
            "42703",
            "column \"z\" of relation \"t\" does not exist",
            Some("16"),
        ),
        (
            "insert into t (a, z) values (1, 2)",
            "42703",
            "column \"z\" of relation \"t\" does not exist",
            Some("19"),
        ),
        (
            "insert into t (a, b) values (1)",
            "42601",
            "INSERT has more target columns than expressions",
            Some("19"),
        ),
        (
            "insert into t (a, b) select 1",
            "42601",
            "INSERT has more target columns than expressions",
            Some("19"),
        ),
        (
            "update t set z = 1",
            "42703",
            "column \"z\" of relation \"t\" does not exist",
            Some("14"),
        ),
        (
            "update t set (a, z) = (1, 2)",
            "42703",
            "column \"z\" of relation \"t\" does not exist",
            Some("18"),
        ),
        ("update t set a = 1, a = 2", "42601", "multiple assignments to same column \"a\"", None),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), position, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn on_conflict_reads_its_action_before_it_matches_a_key() {
    let dirs = Dirs::new("conflict-arbiter");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for sql in [
        "create temp table t (a int, b text)",
        "create temp table u (a int primary key, b text)",
        "insert into t (a) values (1) on conflict do nothing",
        "insert into t (a) values (1) on conflict do nothing",
        "insert into u (a) values (1) on conflict (a, a) do nothing",
    ] {
        assert!(!tags(&client.query(sql)).contains('E'), "{sql}");
    }
    assert_eq!(scalar(&mut client, "select count(*) from t"), "2");
    // The cases of the PostgreSQL 19 oracle. An error in the target or in the DO UPDATE comes
    // before the target is matched to a key.
    let unmatched =
        "there is no unique or exclusion constraint matching the ON CONFLICT specification";
    for (sql, state, message, position) in [
        (
            "insert into t (a) values (1) on conflict (a) do update set z = 1",
            "42703",
            "column \"z\" of relation \"t\" does not exist",
            Some("60"),
        ),
        (
            "insert into t (a) values (1) on conflict (a) do update set b = 'x'",
            "42P10",
            unmatched,
            None,
        ),
        (
            "insert into t (a) values (1) on conflict (q) do update set z = 1",
            "42703",
            "column \"q\" does not exist",
            Some("43"),
        ),
        ("insert into t (a) values (1) on conflict (a) do nothing", "42P10", unmatched, None),
        ("insert into u (a) values (1) on conflict (b) do nothing", "42P10", unmatched, None),
        (
            "insert into u (a) values (1) on conflict (b) do update set z = 1",
            "42703",
            "column \"z\" of relation \"u\" does not exist",
            Some("60"),
        ),
        (
            "insert into u (a) values (1) on conflict (a) do update set a = 1, z = 2",
            "42703",
            "column \"z\" of relation \"u\" does not exist",
            Some("67"),
        ),
        (
            "insert into u (a) values (1) on conflict (a) do update set a = 1, a = 2",
            "42601",
            "multiple assignments to same column \"a\"",
            None,
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(state), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), position, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn an_index_over_the_whole_row_reads_each_column() {
    let dirs = Dirs::new("index-whole-row");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The case of `generated_virtual`. The index reads no column by name, and the database file
    // must still take the table, so the statements after it work.
    for sql in [
        "create table gtest20d (a int, b int)",
        "insert into gtest20d values (1), (1)",
        "create index gtest20d_idx2 on gtest20d ((gtest20d = row (1, 2)))",
        "create table after_it (x int)",
        "insert into after_it values (1)",
    ] {
        assert!(!tags(&client.query(sql)).contains('E'), "{sql}");
    }
    let rows = client.query("select count(*) from gtest20d");
    assert_eq!(
        data_row(rows.iter().find(|message| message.tag == b'D').unwrap())[0],
        Some(b"2".to_vec())
    );
    // An index that reads no column at all is still refused.
    let messages = client.query("create index nothing on gtest20d ((1))");
    assert!(tags(&messages).contains('E'));
    server.stop().unwrap();
}

#[test]
fn an_index_with_no_name_is_named_as_postgres_names_it() {
    let dirs = Dirs::new("index-names");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The cases of the PostgreSQL 19 oracle, and the names that it gives.
    for sql in [
        "create temp table t (a int, b text, \"Mixed\" int)",
        "create index on t (a)",
        "create index on t (a)",
        "create index on t (a, b)",
        "create index on t ((a + 1))",
        "create index on t (lower(b))",
        "create index on t (a, a)",
        "create index on t (\"Mixed\")",
        "create unique index on t (b)",
        "create index on t using btree (a)",
        "create temp table t_b_idx2 (x int)",
        "create index on t (b)",
        "create index on t (b)",
        "create temp table averyveryveryveryveryveryverylongtablenamethatgoesonandonandon \
         (averyveryveryveryveryveryverylongcolumnnamethatgoesonandonandon int)",
        "create index on averyveryveryveryveryveryverylongtablenamethatgoesonandonandon \
         (averyveryveryveryveryveryverylongcolumnnamethatgoesonandonandon)",
    ] {
        assert!(!tags(&client.query(sql)).contains('E'), "{sql}");
    }
    let names = client.query("select index_name from duckdb_indexes() order by index_name");
    let names: Vec<String> = names
        .iter()
        .filter(|message| message.tag == b'D')
        .map(|message| String::from_utf8(data_row(message)[0].clone().unwrap()).unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "averyveryveryveryveryveryvery_averyveryveryveryveryveryvery_idx",
            "t_Mixed_idx",
            "t_a_a1_idx",
            "t_a_b_idx",
            "t_a_idx",
            "t_a_idx1",
            "t_a_idx2",
            "t_b_idx",
            "t_b_idx1",
            "t_b_idx3",
            "t_expr_idx",
            "t_lower_idx",
        ]
    );
    for (sql, position) in [("create index on t (z)", "20"), ("create index on t ((z + 1))", "21")]
    {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42703"), "{sql}");
        assert_eq!(
            messages[0].field(b'M').as_deref(),
            Some("column \"z\" does not exist"),
            "{sql}"
        );
        assert_eq!(messages[0].field(b'P').as_deref(), Some(position), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_select_with_no_targets_gives_rows_with_no_columns() {
    let dirs = Dirs::new("pgnotargets");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query("select from generate_series(1, 3)");
    assert_eq!(tags(&messages), "TDDDCZ");
    assert!(row_shape(&messages[0]).is_empty());
    assert!(data_row(&messages[1]).is_empty());
    assert_eq!(tags(&client.query("select")), "TDCZ");
    // The counts are the ones that PostgreSQL 19 gives.
    for (sql, count) in [
        ("select union select", "1"),
        ("select intersect select", "1"),
        ("select except select", "0"),
        ("select from generate_series(1, 5) union all select from generate_series(1, 3)", "8"),
        ("select from generate_series(1, 5) intersect all select from generate_series(1, 3)", "3"),
        ("select from generate_series(1, 5) except all select from generate_series(1, 3)", "2"),
        ("select from generate_series(1, 5) except select from generate_series(1, 3)", "0"),
        ("select from generate_series(1, 4) s where s > 2", "2"),
        ("select from generate_series(1, 4) s group by s % 2", "2"),
        ("select from generate_series(1, 6) limit 2", "2"),
    ] {
        let wrapped = format!("select count(*)::text from ({sql}) t");
        assert_eq!(scalar(&mut client, &wrapped), count, "{sql}");
    }
    assert_eq!(
        scalar(&mut client, "select exists (select from generate_series(1, 2))::text"),
        "true"
    );
    let messages = client.query("select from generate_series(1, 2) order by 1");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(
        messages[0].field(b'M').as_deref(),
        Some("ORDER BY position 1 is not in select list")
    );
    server.stop().unwrap();
}

#[test]
fn a_recursive_query_takes_search_and_cycle_clauses() {
    let dirs = Dirs::new("pgsearchcycle");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values are the ones that PostgreSQL 19 gives.
    let graph = "with recursive g(f, t) as (values (1, 2), (1, 3), (2, 3), (3, 1)), \
        s(f, t) as (select * from g where f = 1 union all select g.* from g, s where g.f = s.t)";
    assert_eq!(
        scalar(
            &mut client,
            &format!(
                "{graph} search breadth first by f, t set seq \
                cycle f, t set c to 'Y' default 'N' using p \
                select string_agg(f || '-' || t || ':' || c || ':' || seq::text, ' ' \
                order by seq, c) from s"
            )
        ),
        "1-2:N:(0,1,2) 1-3:N:(0,1,3) 2-3:N:(1,2,3) 3-1:N:(1,3,1) 1-2:N:(2,1,2) 1-3:Y:(2,1,3) \
        3-1:N:(2,3,1) 1-2:Y:(3,1,2) 1-3:N:(3,1,3) 2-3:N:(3,2,3) 3-1:Y:(4,3,1) 3-1:Y:(4,3,1)"
    );
    assert_eq!(
        scalar(
            &mut client,
            &format!(
                "{graph} search depth first by f, t set seq cycle f, t set c using p \
                select string_agg(f || '-' || t || ':' || c || ':' || array_length(p, 1), ' ' \
                order by seq, c) from s"
            )
        ),
        "1-2:false:1 2-3:false:2 3-1:false:3 1-2:true:4 1-3:false:4 3-1:true:5 1-3:false:1 \
        3-1:false:2 1-2:false:3 2-3:false:4 3-1:true:5 1-3:true:3"
    );
    // The right side reads the added columns by name, and a star there does not reach them.
    assert_eq!(
        scalar(
            &mut client,
            "with recursive test as (select 0 as x union all select (x + 1) % 4 from test \
            where not is_cycle) cycle x set is_cycle using path \
            select string_agg(x || ':' || is_cycle, ' ') from test"
        ),
        "0:false 1:false 2:false 3:false 0:true"
    );
    assert_eq!(
        scalar(
            &mut client,
            "with recursive a as (select 1 as b union all select * from a) cycle b set c using p \
            select string_agg(b || ' ' || c || ' ' || p::text, '; ') from a"
        ),
        "1 false {(1)}; 1 true {(1),(1)}"
    );
    let counting = "with recursive s(f) as (select 1 union all select f + 1 from s)";
    for (clause, code, message, place) in [
        (
            "search depth first by g set seq",
            "42601",
            "search column \"g\" not in WITH query column list",
            "65",
        ),
        (
            "cycle f set c to true default 55 using p",
            "42804",
            "CYCLE types boolean and integer cannot be matched",
            "95",
        ),
        (
            "cycle f set c to true default false using c",
            "42601",
            "cycle mark column name and cycle path column name are the same",
            "65",
        ),
    ] {
        let sql = format!("{counting} {clause} select * from s");
        let messages = client.query(&sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some(code), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some(place), "{sql}");
    }
    let messages =
        client.query("with s(f) as (select 1) search depth first by f set seq select * from s");
    assert_eq!(tags(&messages), "EZ");
    assert_eq!(messages[0].field(b'M').as_deref(), Some("WITH query is not recursive"));
    server.stop().unwrap();
}

#[test]
fn a_values_row_can_read_a_scalar_query() {
    let dirs = Dirs::new("pgvaluesquery");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The values are the ones that PostgreSQL 19 gives.
    for (sql, expected) in [
        ("values ((select 1))", "1"),
        ("with cte(foo) as (values (42)) values ((select foo from cte))", "42"),
        (
            "select string_agg(a || b, ' ') from (values (1, 'a'), ((select 2), 'b'), (3, 'c'), \
            ((select 5), (select 'e'))) t(a, b)",
            "1a 2b 3c 5e",
        ),
        (
            "select string_agg((select foo::text from (values (f1)) cte(foo)), ' ') \
            from (values (1), (2)) t(f1)",
            "1 2",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), expected, "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_scalar_query_nothing_reads_is_not_run() {
    let dirs = Dirs::new("pgunreadquery");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    let messages = client.query("create temp table i8(q1 int8, q2 int8)");
    assert_eq!(tags(&messages), "CZ");
    let messages = client.query("insert into i8 values (123, 456), (123, 789), (5, 6)");
    assert_eq!(tags(&messages), "CZ");
    // The values and the errors are the ones that PostgreSQL 19 gives.
    for sql in [
        "select string_agg(q1::text, ' ' order by q1) from (select q1, \
        (select q2 from i8 t where t.q1 = i8.q1) as t_sub from i8) s",
        "select string_agg(q1::text, ' ' order by q1) from (with t_cte as materialized \
        (select * from i8 t) select q1, (select q2 from t_cte where t_cte.q1 = i8.q1) as t_sub \
        from i8) s",
    ] {
        assert_eq!(scalar(&mut client, sql), "5 123 123", "{sql}");
    }
    for sql in [
        "select q1, (select q2 from i8 t where t.q1 = i8.q1) from i8",
        "select q1 from (select q1, (select q2 from i8 t where t.q1 = i8.q1) as t_sub from i8) s \
        where t_sub > 0",
        "select q1 from (select distinct q1, (select q2 from i8 t where t.q1 = i8.q1) as t_sub \
        from i8) s",
    ] {
        // The one without a query in `FROM` describes its rows before it fails, as there.
        let messages = client.query(sql);
        let tags = tags(&messages);
        assert!(tags.ends_with("EZ"), "{sql}: {tags}");
        assert_eq!(
            messages[tags.len() - 2].field(b'M').as_deref(),
            Some("more than one row returned by a subquery used as an expression"),
            "{sql}"
        );
    }
    server.stop().unwrap();
}

#[test]
fn a_join_takes_the_names_postgres_gives_it() {
    let dirs = Dirs::new("pgjoinnames");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for sql in [
        "create temp table j1 (i int, j int, t text)",
        "create temp table j2 (i int, k int)",
        "insert into j1 values (1, 4, 'one'), (2, 3, 'two'), (0, null, 'zero')",
        "insert into j2 values (1, -1), (2, 2), (5, -5)",
    ] {
        assert!(tags(&client.query(sql)).ends_with("CZ"), "{sql}");
    }
    // The values and the errors are the ones that PostgreSQL 19 gives.
    for (sql, expected) in [
        (
            "select string_agg(ii || tt || kk, ' ' order by ii, kk) from (j1 cross join j2) \
          as tx (ii, jj, tt, ii2, kk) where ii > 0 and kk > 0",
            "1one2 2two2",
        ),
        (
            "select string_agg(x::text, ' ' order by x.i) from (j1 join j2 using (i)) x",
            "(1,4,one,-1) (2,3,two,2)",
        ),
        (
            "select string_agg(x.i || j1.t, ' ' order by x.i) from j1 join j2 using (i) as x",
            "1one 2two",
        ),
        ("select string_agg(row(x.*)::text, ' ') from j1 join j2 using (i) as x", "(1) (2)"),
        ("select count(*) from (j1 a join j2 b using (i)) as a", "2"),
        (
            "select string_agg(i::text, ' ' order by i) from (j1 full join j2 using (i)) as x",
            "0 1 2 5",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), expected, "{sql}");
    }
    for (sql, message) in [
        (
            "select * from (j1 join j2 using (i)) as x where j1.t = 'one'",
            "invalid reference to FROM-clause entry for table \"j1\"",
        ),
        ("select * from j1 join j2 using (i) as x where x.t = 'one'", "column x.t does not exist"),
        (
            "select * from (j1 join j2 using (i) as x) as xx where x.i = 1",
            "missing FROM-clause entry for table \"x\"",
        ),
        (
            "select * from j1 a1 join j2 a2 using (i) as a1",
            "table name \"a1\" specified more than once",
        ),
        (
            "select * from (j1 join j2 using (i)) as x (a, b, c, d, e)",
            "join expression \"x\" has 4 columns available but 5 columns specified",
        ),
        (
            "select * from (j1 t1 join j2 t2 on t1.i = t2.i) as x where x.i = 1",
            "column reference \"i\" is ambiguous",
        ),
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
    }
    let messages = client.query("select * from (j1 join j2 using (i)) as x where j1.t = 'one'");
    assert_eq!(
        messages[0].field(b'D').as_deref(),
        Some(
            "There is an entry for table \"j1\", but it cannot be referenced from this part of the query."
        )
    );
    server.stop().unwrap();
}

#[test]
fn a_row_compares_a_pair_at_a_time_as_postgres() {
    let dirs = Dirs::new("pgrowcompare");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    for sql in [
        "create temp table rc (f1 int, f2 int)",
        "insert into rc values (1, 2), (2, 3), (1, 1), (8, null)",
    ] {
        assert!(tags(&client.query(sql)).ends_with("CZ"), "{sql}");
    }
    // The values and the errors are the ones that PostgreSQL 19 gives.
    for (sql, expected) in [
        (
            "select string_agg(coalesce((row(1, 2) = (select f1, f2))::text, 'null'), ' ' \
             order by f1, f2) from rc",
            "false true false false",
        ),
        ("select row(1, 2) = (select f1, f2 from rc where f1 = 1 and f2 = 2)", "t"),
        (
            "select coalesce((row(1, 2) = (select f1, f2 from rc where false))::text, 'null')",
            "null",
        ),
        ("select row(1, 2) < (select 1, 3)", "t"),
        ("select coalesce((row(1, null) = row(2, 2))::text, 'null')", "false"),
        ("select coalesce((row(1, null) = row(1, 2))::text, 'null')", "null"),
        ("select row(1, null) < row(2, null)", "t"),
        ("select coalesce((row(1, null) < row(1, 2))::text, 'null')", "null"),
        ("select row(1, 2) >= row(1, 2)", "t"),
        ("select row(2, 0) > row(1, 9)", "t"),
        ("select coalesce((row(1, 2) <> row(1, null::int))::text, 'null')", "null"),
    ] {
        assert_eq!(scalar(&mut client, sql), expected, "{sql}");
    }
    for (sql, message) in [
        (
            "select row(1, 2) = (select f1, f2 from rc)",
            "more than one row returned by a subquery used as an expression",
        ),
        ("select (1, 2) = (select 1, 2, 3)", "subquery has too many columns"),
        ("select (1, 2, 3) = (select 1, 2)", "subquery has too few columns"),
        ("select row(1, 2) = row(1, 2, 3)", "unequal number of entries in row expressions"),
        ("select row() = row()", "cannot compare rows of zero length"),
    ] {
        let messages = client.query(sql);
        let tags = tags(&messages);
        assert!(tags.ends_with("EZ"), "{sql}");
        let error = &messages[tags.len() - 2];
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn fetch_first_with_ties_keeps_the_rows_that_tie_as_postgres() {
    let dirs = Dirs::new("pgwithties");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The answers and the errors are the ones that PostgreSQL 19 gives.
    for (sql, expected) in [
        (
            "select string_agg(x::text, ' ') from (select x from generate_series(1, 9) x \
             order by x / 3 fetch first 3 rows with ties) s",
            "1 2 3 4 5",
        ),
        (
            "select string_agg(x::text, ' ') from (select x from generate_series(1, 9) x \
             order by x / 3 offset 3 fetch first 1 row with ties) s",
            "4 5",
        ),
        (
            "select count(*)::text from (select x from generate_series(1, 9) x \
             order by x / 3 fetch first 0 rows with ties) s",
            "0",
        ),
        (
            "select count(*)::text from (select x from generate_series(1, 9) x \
             order by x / 3 fetch first (select null::int) rows with ties) s",
            "9",
        ),
        // The rows that tie run on past the end of the first chunk.
        (
            "select count(*)::text from (select x from generate_series(1, 9000) x \
             order by x / 3000 fetch first 1 row with ties) s",
            "2999",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), expected, "{sql}");
    }
    for (sql, message) in [
        (
            "select x from generate_series(1, 9) x order by x fetch first null rows with ties",
            "row count cannot be null in FETCH FIRST ... WITH TIES clause",
        ),
        (
            "select x from generate_series(1, 9) x fetch first 1 row with ties",
            "WITH TIES cannot be specified without ORDER BY clause",
        ),
    ] {
        let messages = client.query(sql);
        let tags = tags(&messages);
        assert!(tags.ends_with("EZ"), "{sql}");
        let error = &messages[tags.len() - 2];
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_limit_in_a_correlated_subquery_reads_the_outer_row_as_postgres() {
    let dirs = Dirs::new("pgcorrlim");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The answers and the errors are the ones that PostgreSQL 19 gives.
    for (sql, expected) in [
        (
            "select string_agg((select n from generate_series(1, 10) n order by n \
             limit 1 offset s - 1)::text, ' ' order by s) from generate_series(1, 3) s",
            "1 2 3",
        ),
        (
            "select string_agg((select count(*) from (select n from generate_series(1, 10) n \
             limit nullif(s, 3)) t)::text, ' ' order by s) from generate_series(2, 4) s",
            "2 10 4",
        ),
        (
            "select string_agg((select count(*) from (select n from generate_series(1, 10) n \
             offset nullif(s, 3)) t)::text, ' ' order by s) from generate_series(2, 4) s",
            "8 10 6",
        ),
        (
            "select string_agg((select string_agg(n::text, ',') from (select n \
             from generate_series(1, 10) n order by n / 3 fetch first s rows with ties) t), ' ' \
             order by s) from generate_series(1, 3) s",
            "1,2 1,2 1,2,3,4,5",
        ),
    ] {
        assert_eq!(scalar(&mut client, sql), expected, "{sql}");
    }
    for (sql, message) in [
        (
            "select (select count(*) from (select n from generate_series(1, 10) n \
             limit s - 3) t) from generate_series(2, 4) s",
            "LIMIT must not be negative",
        ),
        (
            "select (select count(*) from (select n from generate_series(1, 10) n \
             offset s - 3) t) from generate_series(2, 4) s",
            "OFFSET must not be negative",
        ),
    ] {
        let messages = client.query(sql);
        let tags = tags(&messages);
        assert!(tags.ends_with("EZ"), "{sql}");
        let error = &messages[tags.len() - 2];
        assert_eq!(error.field(b'M').as_deref(), Some(message), "{sql}");
    }
    server.stop().unwrap();
}

#[test]
fn a_volatile_target_is_computed_after_the_sort_as_postgres() {
    let dirs = Dirs::new("pgsorttgt");
    let server = Server::start(dirs.config()).unwrap();
    let mut client = Client::unix(&server);
    connect(&mut client, PROTOCOL_3_0);
    // The answers are the ones that PostgreSQL 19 gives. A target that is not sorted on is
    // computed for the rows that come out of the sort, and for the rows that an offset skips.
    for (sql, expected) in [
        ("create temp sequence s1", None),
        (
            "select string_agg(n::text, ' ' order by n) from (select g, nextval('s1') n \
             from generate_series(1, 30) g order by (g * 7) % 13, g limit 3) q",
            Some("1 2 3"),
        ),
        ("select currval('s1')", Some("3")),
        ("create temp sequence s2", None),
        (
            "select string_agg(n::text, ' ' order by n) from (select g, nextval('s2') n \
             from generate_series(1, 30) g order by g desc limit 2 offset 3) q",
            Some("4 5"),
        ),
        ("select currval('s2')", Some("5")),
        ("create temp sequence s3", None),
        (
            "select string_agg(n::text, ' ' order by n) from (select nextval('s3') n \
             from generate_series(1, 30) g offset 27) q",
            Some("28 29 30"),
        ),
        (
            "select string_agg(g::text || ':' || n, ' ') from (select g, nextval('s3') - 30 n \
             from generate_series(1, 30) g order by g desc limit 3) q",
            Some("30:1 29:2 28:3"),
        ),
    ] {
        match expected {
            Some(expected) => assert_eq!(scalar(&mut client, sql), expected, "{sql}"),
            None => assert!(tags(&client.query(sql)).ends_with("CZ"), "{sql}"),
        }
    }
    client.query("create temp sequence s4");
    let messages = client.query("select currval('s4')");
    let tags = tags(&messages);
    assert!(tags.ends_with("EZ"), "{tags}");
    let error = &messages[tags.len() - 2];
    assert_eq!(error.field(b'C').as_deref(), Some("55000"));
    assert_eq!(
        error.field(b'M').as_deref(),
        Some("currval of sequence \"s4\" is not yet defined in this session")
    );
    server.stop().unwrap();
}
