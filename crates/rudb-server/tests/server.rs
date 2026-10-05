//! The server end to end, with a raw client over a Unix socket and over TCP. The messages and
//! their order are those of the PostgreSQL 19 oracle for the same bytes.

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
    assert_eq!(parameter_types(&messages[1]), [23, 25]);

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
