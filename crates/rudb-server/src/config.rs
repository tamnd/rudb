//! The settings of the server and the command line that sets them.
//!
//! The names, the defaults and the syntax of the values are those of PostgreSQL, so a script that
//! starts `postgres` with `-D`, `-p`, `-h`, `-k`, `-N` and `-c name=value` starts `rudb-server`
//! the same way. `postgresql.conf` comes later. Until then the built-in defaults and the command
//! line are the only sources.

use std::path::PathBuf;

/// The settings that the server reads at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The data directory, `-D` or `PGDATA`.
    pub data: PathBuf,
    /// `listen_addresses`: host names or addresses with commas between them, `*` for all, or
    /// empty for no TCP.
    pub listen_addresses: String,
    /// `port`, for TCP and for the name of the Unix socket. 0 lets the system choose a port for
    /// TCP, which the tests use.
    pub port: u16,
    /// `unix_socket_directories`, with commas between them, or empty for no Unix socket.
    pub unix_socket_directories: String,
    /// `unix_socket_permissions`.
    pub unix_socket_permissions: u32,
    /// `max_connections`.
    pub max_connections: usize,
    /// `auto_create_database`: create a missing database at connect time. Off by default, as the
    /// behavior of PostgreSQL.
    pub auto_create_database: bool,
}

impl Config {
    /// The defaults of PostgreSQL for the data directory `data`.
    pub fn new(data: impl Into<PathBuf>) -> Config {
        Config {
            data: data.into(),
            listen_addresses: "localhost".to_owned(),
            port: 5432,
            unix_socket_directories: "/tmp".to_owned(),
            unix_socket_permissions: 0o777,
            max_connections: 100,
            auto_create_database: false,
        }
    }

    /// Sets one setting by name, with no regard to case, from the text of its value.
    ///
    /// # Errors
    ///
    /// The text of PostgreSQL for a name that it does not know or a value that is not correct.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let invalid = || format!("invalid value for parameter \"{name}\": \"{value}\"");
        match name.to_ascii_lowercase().as_str() {
            "listen_addresses" => self.listen_addresses = value.to_owned(),
            "port" => {
                self.port =
                    parse_int(value).and_then(|v| u16::try_from(v).ok()).ok_or_else(invalid)?
            }
            "unix_socket_directories" => self.unix_socket_directories = value.to_owned(),
            "unix_socket_permissions" => {
                self.unix_socket_permissions = parse_int(value)
                    .filter(|v| (0..=0o777).contains(v))
                    .ok_or_else(invalid)? as u32;
            }
            "max_connections" => {
                self.max_connections = parse_int(value)
                    .filter(|v| (1..=262_143).contains(v))
                    .ok_or_else(invalid)? as usize;
            }
            "auto_create_database" => {
                self.auto_create_database = parse_bool(value)
                    .ok_or_else(|| format!("parameter \"{name}\" requires a Boolean value"))?;
            }
            _ => return Err(format!("unrecognized configuration parameter \"{name}\"")),
        }
        Ok(())
    }

    /// Reads the command line of `postgres`: `-D dir`, `-p port`, `-h addresses`, `-k dirs`,
    /// `-N max`, `-c name=value` and `--name=value`. A dash in a name is an underscore, as in
    /// PostgreSQL. Without `-D` the data directory is `PGDATA`.
    ///
    /// # Errors
    ///
    /// An option that is not known, an option without its value, or a value that [`Config::set`]
    /// refuses.
    pub fn from_args(args: &[String]) -> Result<Config, String> {
        let mut data = None;
        let mut settings = Vec::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = |option: &str| {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("option requires an argument -- '{option}'"))
            };
            match arg.as_str() {
                "-D" => data = Some(PathBuf::from(value("D")?)),
                "-p" => settings.push(("port".to_owned(), value("p")?)),
                "-h" => settings.push(("listen_addresses".to_owned(), value("h")?)),
                "-k" => settings.push(("unix_socket_directories".to_owned(), value("k")?)),
                "-N" => settings.push(("max_connections".to_owned(), value("N")?)),
                "-c" => settings.push(name_value(&value("c")?)?),
                _ if arg.starts_with("--") => settings.push(name_value(&arg[2..])?),
                _ => return Err(format!("invalid argument: \"{arg}\"")),
            }
        }
        let data = data.or_else(|| std::env::var_os("PGDATA").map(PathBuf::from)).ok_or_else(|| {
            "rudb-server does not know where to find the data directory.\nYou must specify the -D \
             invocation option or set the PGDATA environment variable."
                .to_owned()
        })?;
        let mut config = Config::new(data);
        for (name, value) in settings {
            config.set(&name, &value)?;
        }
        Ok(config)
    }
}

/// `name=value`, with each dash in the name as an underscore.
fn name_value(arg: &str) -> Result<(String, String), String> {
    match arg.split_once('=') {
        Some((name, value)) => Ok((name.replace('-', "_"), value.to_owned())),
        None => Err(format!("--{arg} requires a value")),
    }
}

/// An integer as `parse_int` of `guc.c` reads it: decimal, octal with a leading zero, or
/// hexadecimal with `0x`, with spaces around it.
fn parse_int(value: &str) -> Option<i64> {
    let value = value.trim();
    let (negative, digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let magnitude =
        if let Some(hex) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
            i64::from_str_radix(hex, 16).ok()?
        } else if digits.len() > 1 && digits.starts_with('0') {
            i64::from_str_radix(&digits[1..], 8).ok()?
        } else {
            digits.parse().ok()?
        };
    Some(if negative { -magnitude } else { magnitude })
}

/// `parse_bool` of PostgreSQL: a prefix of `true`, `false`, `yes` or `no`, `on`, `off` or a
/// prefix of `off` of two letters or more, `1` or `0`, in any case.
fn parse_bool(value: &str) -> Option<bool> {
    let value = value.trim().to_ascii_lowercase();
    let prefix_of = |word: &str, min: usize| value.len() >= min && word.starts_with(&value);
    if prefix_of("true", 1) || prefix_of("yes", 1) || value == "on" || value == "1" {
        Some(true)
    } else if prefix_of("false", 1) || prefix_of("no", 1) || prefix_of("off", 2) || value == "0" {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_command_line_of_postgres() {
        let config = Config::from_args(&args(
            "-D /d -p 0x10 -h * -k /a,/b -c max-connections=7 --auto-create-database=on",
        ))
        .unwrap();
        assert_eq!(config.data, PathBuf::from("/d"));
        assert_eq!(config.port, 16);
        assert_eq!(config.listen_addresses, "*");
        assert_eq!(config.unix_socket_directories, "/a,/b");
        assert_eq!(config.max_connections, 7);
        assert!(config.auto_create_database);
        let error = Config::from_args(&args("-D /d -c nope=1")).unwrap_err();
        assert_eq!(error, "unrecognized configuration parameter \"nope\"");
        let error = Config::from_args(&args("-D /d -p 70000")).unwrap_err();
        assert_eq!(error, "invalid value for parameter \"port\": \"70000\"");
        assert!(Config::from_args(&args("-D")).is_err());
    }

    #[test]
    fn integers_and_booleans_as_postgres_reads_them() {
        assert_eq!(parse_int("0777"), Some(511));
        assert_eq!(parse_int(" 12 "), Some(12));
        assert_eq!(parse_int("0"), Some(0));
        assert_eq!(parse_int("09"), None);
        for on in ["t", "TRUE", "y", "on", "1"] {
            assert_eq!(parse_bool(on), Some(true), "{on}");
        }
        for off in ["f", "no", "of", "OFF", "0"] {
            assert_eq!(parse_bool(off), Some(false), "{off}");
        }
        assert_eq!(parse_bool("o"), None);
    }
}
