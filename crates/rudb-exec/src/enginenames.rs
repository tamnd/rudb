//! The tables that describe the engine itself rather than anything somebody stored in it.
//!
//! Six of them: `duckdb_extensions()`, `duckdb_optimizers()`, `pragma_version()`,
//! `pragma_platform()`, `pragma_user_agent()` and `pragma_database_size()`. Every one is something a
//! tool reads on connect to find out what it is talking to, and every one is a place where rudb has
//! to answer about itself rather than reproduce what the pin says. That is the rule this whole file
//! is built on and the four pragmas argue it again where they are written: a client asking which
//! engine this is deserves the answer, and compatibility means the same query gives the same answer,
//! not that a version string is forged.
//!
//! The first two come out different ways round and it is worth saying why.
//!
//! `duckdb_optimizers()` is the whole of DuckDB's list, all forty four, because the table means "the
//! names `SET disabled_optimizers` takes" and rudb takes all forty four. `rudb_opt::UPSTREAM` is
//! already that list and already the thing the setting is checked against, so this table reads the
//! same constant rather than a second copy of it. Accepting a name for a pass rudb has not written
//! is not a pretence: turning off a pass that does not exist is a request that has already been
//! granted, and refusing it would fail a `SET` and end a corpus file over a pass whose absence
//! changes no answer. `rudb_opt::PASSES` is the eight that are actually written and it is not what
//! this table returns, because a client reading this table is asking what it may name.
//!
//! `duckdb_extensions()` is the other way round. The names and the descriptions and the aliases are
//! the pin's, because they are facts about the extensions rather than about the engine, and the two
//! boolean columns are rudb's own answer. `parquet` and `core_functions` are loaded and installed
//! here, because `read_parquet` and `COPY TO` really do read and write Parquet and the function
//! library really is there, and everything else is `NOT_INSTALLED`. That includes three the pin has
//! statically linked. `icu` is false because there is no session time zone and no collation, which
//! is a later box on the same milestone. `json` is false because `rudb-json` is a scaffold. `shell`
//! and `autocomplete` are false because they are the DuckDB shell's and rudb's command line tool is
//! not that shell.
//!
//! A row for an extension rudb does not have is worth returning rather than leaving out. A tool that
//! asks whether `spatial` is available gets false, which is the answer, where an empty table would
//! make it guess.
//!
//! The empty strings are measured and are not nulls. An extension that is not installed reports an
//! empty `install_path`, an empty `extension_version` and an empty `installed_from`, and only
//! `signature_key_fingerprint` is null, on every row including the loaded ones.
//!
//! The extension rows come out in the pin's own order without anything being done about it, because
//! the pin returns them sorted by name and the list is written sorted. The optimizer rows do not:
//! the pin returns them in the order its pipeline runs the passes in and rudb returns them sorted,
//! which is the same divergence `crate::keywords` has and it is left for the same reason. The order
//! a list happens to be built in is not a fact about the language, and a sqllogictest record that
//! cares about order says so.

use rudb_catalog::Catalog;
use rudb_common::{LogicalType, Memory, Result, Value, human};
use rudb_functions::{
    database_size_fields, extension_fields, optimizer_fields, platform_fields, user_agent_fields,
    version_fields,
};
use rudb_opt::UPSTREAM;
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// What a built in extension reports as its `install_path`, which is the pin's spelling.
const BUILT_IN: &str = "(BUILT-IN)";

/// Every extension DuckDB's default build advertises, and whether rudb has it.
///
/// The name, then whether rudb provides it, then the aliases, then the description. The aliases and
/// the description are the pin's word for word, because they say what the extension is rather than
/// what this engine does about it.
const EXTENSIONS: &[(&str, bool, &[&str], &str)] = &[
    ("autocomplete", false, &[], "Adds support for autocomplete in the shell"),
    ("avro", false, &[], "Adds support for reading Avro files"),
    ("aws", false, &[], "Provides features that depend on the AWS SDK"),
    ("azure", false, &[], "Adds a filesystem abstraction for Azure blob storage to DuckDB"),
    ("core_functions", true, &[], "Core function library"),
    ("delta", false, &[], "Adds support for Delta Lake"),
    ("ducklake", false, &[], "Adds support for DuckLake, SQL as a Lakehouse Format"),
    ("encodings", false, &[], "All unicode encodings to UTF-8"),
    ("excel", false, &[], "Adds support for Excel-like format strings"),
    ("fts", false, &[], "Adds support for Full-Text Search Indexes"),
    (
        "httpfs",
        false,
        &["http", "https", "s3"],
        "Adds support for reading and writing files over a HTTP(S) connection",
    ),
    ("iceberg", false, &[], "Adds support for Apache Iceberg"),
    ("icu", false, &[], "Adds support for time zones and collations using the ICU library"),
    ("inet", false, &[], "Adds support for IP-related data types and functions"),
    ("json", false, &[], "Adds support for JSON operations"),
    ("lance", false, &[], "Adds support for querying Lance datasets"),
    ("motherduck", false, &["md"], "Enables motherduck integration with the system"),
    ("mysql_scanner", false, &["mysql"], "Adds support for connecting to a MySQL database"),
    ("odbc_scanner", false, &["odbc"], "Adds support for connecting to remote databases over ODBC"),
    ("parquet", true, &[], "Adds support for reading and writing parquet files"),
    (
        "postgres_scanner",
        false,
        &["postgres"],
        "Adds support for connecting to a Postgres database",
    ),
    ("quack", false, &[], "The DuckDB 'Quack' Client/Server Protocol"),
    ("shell", false, &[], "Adds CLI-specific support and functionalities"),
    (
        "spatial",
        false,
        &[],
        "Geospatial extension that adds support for working with spatial data and functions",
    ),
    (
        "sqlite_scanner",
        false,
        &["sqlite", "sqlite3"],
        "Adds support for reading and writing SQLite database files",
    ),
    ("tpcds", false, &[], "Adds TPC-DS data generation and query support"),
    ("tpch", false, &[], "Adds TPC-H data generation and query support"),
    ("ui", false, &[], "Adds local UI for DuckDB"),
    ("unity_catalog", false, &["uc_catalog"], "Adds support for connecting to Unity Catalog"),
    (
        "vortex",
        false,
        &[],
        "Adds support for reading and writing files using the Vortex file format",
    ),
    ("vss", false, &[], "Adds indexing support to accelerate Vector Similarity Search"),
];

/// Every extension and whether this engine has it, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn extensions(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let mut rows = Vec::with_capacity(EXTENSIONS.len());
    for (name, held, aliases, description) in EXTENSIONS {
        let version = if *held { env!("CARGO_PKG_VERSION") } else { "" };
        rows.push(vec![
            text(name),
            Value::Boolean(*held),
            Value::Boolean(*held),
            text(if *held { BUILT_IN } else { "" }),
            text(description),
            Value::List {
                element: LogicalType::Varchar,
                values: aliases.iter().map(|alias| text(alias)).collect(),
            },
            text(version),
            text(if *held { "STATICALLY_LINKED" } else { "NOT_INSTALLED" }),
            text(""),
            Value::Null,
        ]);
    }
    Metadata::new("duckdb_extensions", &extension_fields(), &rows, plan, index, columns)
}

/// Every name `SET disabled_optimizers` takes, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn optimizers(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let rows: Vec<Vec<Value>> = UPSTREAM.iter().map(|name| vec![text(name)]).collect();
    Metadata::new("duckdb_optimizers", &optimizer_fields(), &rows, plan, index, columns)
}

/// What this build calls itself, which is the crate version with the `v` the pin writes.
///
/// rudb answers about rudb here and does not report a DuckDB version, and that is a decision rather
/// than an oversight. A client reading `library_version` is asking which engine it is talking to so
/// it can decide what to send, and an engine that answers with somebody else's version number has
/// made that decision impossible to get right for everybody who asks. The compatibility this project
/// is after is that the same query gives the same answer, not that a fingerprint is forged.
fn library_version() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// The revision this build was made from, or nothing when the build was not told.
///
/// The pin always has one because DuckDB's build system puts the git hash in. rudb has no build
/// script anywhere and is not getting one for this, so the value is read out of the environment at
/// compile time and is empty in an ordinary `cargo build`. An empty string is the honest answer to
/// "which commit is this" from a build that was never told, and it is better than a made up one,
/// because a made up revision is worse than no revision to anybody trying to reproduce a bug.
fn source_id() -> &'static str {
    option_env!("RUDB_SOURCE_ID").unwrap_or("")
}

/// The name of this release, which is `Development Version` and will be for a while.
///
/// DuckDB names its releases after birds and calls a build between releases a development version.
/// rudb is pre 1.0 and every build of it is a development version, so the pin's own words for that
/// state are the true ones here as well.
const CODENAME: &str = "Development Version";

/// The operating system and processor this build was made for, in DuckDB's spelling.
///
/// The spelling matters because it is what an extension is published under, so `osx` rather than
/// `macos` and `amd64` rather than `x86_64` are not a preference, they are the names in the paths.
/// A target neither list knows is written out as Rust spells it, because a wrong guess at a name
/// somebody downloads from is worse than an unfamiliar one.
fn platform_name() -> String {
    let os = match std::env::consts::OS {
        "macos" => "osx",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}_{arch}")
}

/// The version of the engine answering, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn version(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let rows = vec![vec![text(&library_version()), text(source_id()), text(CODENAME)]];
    Metadata::new("pragma_version", &version_fields(), &rows, plan, index, columns)
}

/// The platform this build was made for, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn platform(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let rows = vec![vec![text(&platform_name())]];
    Metadata::new("pragma_platform", &platform_fields(), &rows, plan, index, columns)
}

/// The line a client sends to say who it is, in the columns the plan asked for.
///
/// The pin writes `duckdb/<version>(<platform>) cli` from its shell, where the last word is the
/// client naming itself. rudb has no way for a client to name itself yet, so the line stops after
/// the platform, and the day there is one it goes on the end in the same place.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn user_agent(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let written = format!("rudb/{}({})", library_version(), platform_name());
    let rows = vec![vec![text(&written)]];
    Metadata::new("pragma_user_agent", &user_agent_fields(), &rows, plan, index, columns)
}

/// What each attached database costs, in the columns the plan asked for.
///
/// One row per database somebody can create in, which is every attached database that is not one of
/// the engine's own, and that is the pin's answer too: a fresh session reports `memory` and neither
/// `system` nor `temp`.
///
/// Seven of the nine columns are about a file and rudb has no files, so they are zero. That is not a
/// placeholder, it is the size of a database that lives entirely in memory, and the pin says exactly
/// the same about its own in memory database down to the `0 bytes` spelling. The day rudb writes a
/// database file these columns start meaning something without their definition changing.
///
/// The two that are answered are answered from the live budget rather than from the configuration,
/// so a `SET memory_limit` shows up here as it does in `current_setting`, and the usage is what the
/// query being built is allowed to see: read once when the operator is built, like every other
/// metadata table, so that a reservation taken between two chunks does not put two numbers in one
/// result.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn database_size(
    catalog: &Catalog,
    memory: &Memory,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let limit = memory.limit().map_or_else(|| "unlimited".to_string(), human);
    let used = human(memory.used());
    let nothing = human(0);
    let mut rows = Vec::new();
    for database in catalog.databases() {
        if database.internal() {
            continue;
        }
        rows.push(vec![
            text(database.name()),
            text(&nothing),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            text(&nothing),
            text(&used),
            text(&limit),
        ]);
    }
    Metadata::new("pragma_database_size", &database_size_fields(), &rows, plan, index, columns)
}

#[cfg(test)]
mod tests {
    use super::{CODENAME, EXTENSIONS, library_version, platform_name, source_id};

    /// The pin returns thirty one rows and so does this, and the names are sorted there and here.
    #[test]
    fn the_extension_list_is_the_pins_list_in_the_pins_order() {
        assert_eq!(EXTENSIONS.len(), 31);
        let mut sorted: Vec<&str> = EXTENSIONS.iter().map(|(name, ..)| *name).collect();
        let written = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(written, sorted);
    }

    /// Two are held and the rest are not, and a held one is one somebody can point at.
    #[test]
    fn the_two_extensions_this_engine_claims_are_the_two_it_has() {
        let held: Vec<&str> =
            EXTENSIONS.iter().filter(|(_, held, ..)| *held).map(|(name, ..)| *name).collect();
        assert_eq!(held, vec!["core_functions", "parquet"]);
    }

    /// The version is this crate's with the pin's `v` on the front and nothing else in it.
    #[test]
    fn the_version_is_written_the_way_the_pin_writes_one() {
        let written = library_version();
        let number = written.strip_prefix('v').expect("a leading v, which is the pin's spelling");
        assert_eq!(number, env!("CARGO_PKG_VERSION"));
        assert_eq!(number.split('.').count(), 3);
        assert!(number.split('.').all(|part| part.parse::<u32>().is_ok()), "{written}");
    }

    /// A build nobody told a revision to says nothing rather than making one up.
    #[test]
    fn the_source_id_is_empty_unless_the_build_was_told_one() {
        let written = source_id();
        assert!(written.is_empty() || written.chars().all(|c| c.is_ascii_hexdigit()), "{written}");
        assert_eq!(CODENAME, "Development Version");
    }

    /// The platform is two words joined by an underscore, in the spelling a download path uses.
    #[test]
    fn the_platform_is_the_os_and_the_processor_in_duckdbs_spelling() {
        let written = platform_name();
        let (os, arch) = written.split_once('_').expect("an os and an arch");
        assert!(!os.is_empty() && !arch.is_empty(), "{written}");
        // Neither of the two Rust spellings that DuckDB does not use may reach the string, because
        // the whole point of the mapping is that this is the name an extension is published under.
        assert_ne!(os, "macos");
        assert_ne!(arch, "x86_64");
        assert_ne!(arch, "aarch64");
    }
}
