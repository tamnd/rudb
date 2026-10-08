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
    assert_eq!(tags(&client.query("commit")), "NCZ");
    client.query("set client_min_messages = error");
    assert_eq!(tags(&client.query("commit")), "CZ");
    client.query("reset client_min_messages");
    assert_eq!(tags(&client.query("drop table if exists nx")), "NCZ");
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
    ] {
        let messages = client.query(sql);
        assert_eq!(tags(&messages), "EZ", "{sql}");
        assert_eq!(messages[0].field(b'C').as_deref(), Some("42883"), "{sql}");
        assert_eq!(messages[0].field(b'M').as_deref(), Some(message), "{sql}");
        assert_eq!(messages[0].field(b'D').as_deref(), Some(detail), "{sql}");
        assert_eq!(messages[0].field(b'P').as_deref(), Some("8"), "{sql}");
    }
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
