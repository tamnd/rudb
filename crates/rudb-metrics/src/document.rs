//! The document itself: what is in it, and what it looks like written down.
//!
//! Every type here is plain data with public fields and a constructor. The fields are public
//! because whoever measures a thing is the only one who knows its number, and the constructor is
//! there so that a field added later does not break the caller that only wanted the id and the
//! kind.
//!
//! Nothing here measures anything. There is no clock in this crate and no counter, which is on
//! purpose: this is the shape the numbers are reported in, and the code that produces them is the
//! instrumentation shim that sits around the push operators.

use crate::SCHEMA;
use crate::json::Writer;

/// Everything one execution reports about itself.
#[derive(Debug, Clone)]
pub struct Document {
    /// What was run.
    pub query: Query,
    /// What ran it.
    pub engine: Engine,
    /// Where it ran.
    pub machine: Machine,
    /// What it was allowed.
    pub settings: Settings,
    /// How it ended.
    pub outcome: Outcome,
    /// Where the time went before and during execution.
    pub timing: Timing,
    /// What it used.
    pub resource: Resource,
    /// What was chosen at each seam, and who chose it.
    pub strategies: Vec<Strategy>,
    /// One row per pipeline.
    pub pipelines: Vec<Pipeline>,
    /// One row per operator.
    pub operators: Vec<Operator>,
}

impl Document {
    /// A document for this statement, with the engine and the machine filled in and every number
    /// still zero.
    #[must_use]
    pub fn new(sql: &str) -> Self {
        Self {
            query: Query::new(sql),
            engine: Engine::here(),
            machine: Machine::here(),
            settings: Settings::default(),
            outcome: Outcome::Succeeded,
            timing: Timing::default(),
            resource: Resource::default(),
            strategies: Vec::new(),
            pipelines: Vec::new(),
            operators: Vec::new(),
        }
    }

    /// The document as JSON, with the generated warnings on the end of it.
    ///
    /// This is the whole of the output format. `--metrics run.json` writes this to a file,
    /// `rudb-bench` parses it, and `EXPLAIN ANALYZE` prints the same numbers in a shape meant for a
    /// terminal rather than for a parser.
    #[must_use]
    pub fn render(&self) -> String {
        Writer::document(|out| self.write(out))
    }

    /// The same document on one line, which is what the shell appends under `--metrics`.
    ///
    /// A document per line makes a file of many of them readable a record at a time, which is what a
    /// harness wants when it ran a setup statement and a query and only cares about the second.
    #[must_use]
    pub fn one_line(&self) -> String {
        Writer::one_line(|out| self.write(out))
    }

    /// The keys, in the order the schema lists them, in whichever shape the writer is in.
    fn write(&self, out: &mut Writer) {
        out.count("schema", u64::from(SCHEMA));
        out.key("query");
        out.object(|out| {
            out.words("sql", &self.query.sql);
            out.words("hash", &self.query.hash);
            out.maybe_words("suite", self.query.suite.as_deref());
            out.maybe_words("id", self.query.id.as_deref());
        });
        out.key("engine");
        out.object(|out| {
            out.words("version", &self.engine.version);
            out.maybe_words("commit", self.engine.commit.as_deref());
            out.words("build", &self.engine.build);
        });
        out.key("machine");
        out.object(|out| {
            out.maybe_words("name", self.machine.name.as_deref());
            out.count("cores", u64::from(self.machine.cores));
            out.maybe_count("memory", self.machine.memory);
            out.words("os", &self.machine.os);
        });
        out.key("settings");
        out.object(|out| {
            out.maybe_count("memory_limit", self.settings.memory_limit);
            out.count("threads", u64::from(self.settings.threads));
        });
        out.key("outcome");
        out.object(|out| {
            out.words("state", self.outcome.state());
            out.maybe_words("message", self.outcome.message());
        });
        out.key("timing");
        out.object(|out| {
            out.count("parse_ns", self.timing.parse_ns);
            out.count("bind_ns", self.timing.bind_ns);
            out.count("optimize_ns", self.timing.optimize_ns);
            out.count("physical_ns", self.timing.physical_ns);
            out.count("execute_ns", self.timing.execute_ns);
            out.count("total_ns", self.timing.total_ns);
        });
        out.key("resource");
        out.object(|out| {
            out.count("cpu_ns", self.resource.cpu_ns);
            out.count("peak_bytes", self.resource.peak_bytes);
            out.count("bytes_read", self.resource.bytes_read);
            out.count("bytes_decoded", self.resource.bytes_decoded);
            out.count("bytes_spilled", self.resource.bytes_spilled);
            out.count("bytes_read_back", self.resource.bytes_read_back);
            out.count("io_requests", self.resource.io_requests);
        });
        out.key("strategies");
        out.array(|out| {
            for strategy in &self.strategies {
                out.item();
                out.object(|out| {
                    out.words("seam", &strategy.seam);
                    out.words("chosen", &strategy.chosen);
                    out.words("by", &strategy.by);
                    out.maybe_words("provenance", strategy.provenance.as_deref());
                });
            }
        });
        out.key("pipelines");
        out.array(|out| {
            for pipeline in &self.pipelines {
                out.item();
                out.object(|out| {
                    out.count("id", u64::from(pipeline.id));
                    out.count("instances", u64::from(pipeline.instances));
                    out.key("depends_on");
                    out.array(|out| {
                        for on in &pipeline.depends_on {
                            out.item();
                            out.number(u64::from(*on));
                        }
                    });
                    out.count("wall_ns", pipeline.wall_ns);
                    out.count("cpu_ns", pipeline.cpu_ns);
                    out.key("blocked_ns");
                    out.object(|out| {
                        out.count("io", pipeline.blocked.io_ns);
                        out.count("memory", pipeline.blocked.memory_ns);
                        out.count("dependency", pipeline.blocked.dependency_ns);
                        out.count("downstream", pipeline.blocked.downstream_ns);
                    });
                });
            }
        });
        out.key("operators");
        out.array(|out| {
            for operator in &self.operators {
                out.item();
                out.object(|out| {
                    out.count("id", u64::from(operator.id));
                    out.count("pipeline", u64::from(operator.pipeline));
                    out.words("kind", &operator.kind);
                    out.maybe_words("detail", operator.detail.as_deref());
                    out.count("rows_in", operator.rows_in);
                    out.count("rows_out", operator.rows_out);
                    out.maybe_count("estimated_rows", operator.estimated_rows);
                    out.count("wall_ns", operator.wall_ns);
                    out.count("cpu_ns", operator.cpu_ns);
                    out.count("bytes_read", operator.bytes_read);
                    out.count("bytes_decoded", operator.bytes_decoded);
                    out.count("bytes_spilled", operator.bytes_spilled);
                    out.key("memory");
                    out.object(|out| {
                        out.count("reserved", operator.memory.reserved);
                        out.count("high_water", operator.memory.high_water);
                    });
                    out.flag("reference_impl", operator.reference_impl);
                });
            }
        });
        out.key("warnings");
        out.array(|out| {
            for warning in self.warnings() {
                out.item();
                out.text(&warning);
            }
        });
    }
}

/// The statement that was run.
#[derive(Debug, Clone)]
pub struct Query {
    /// The SQL as it was given, not as it was rewritten.
    pub sql: String,
    /// A hash of that text, so that two runs of the same statement group together.
    pub hash: String,
    /// The suite it came from, when a harness ran it.
    pub suite: Option<String>,
    /// The name it has in that suite, such as `q32`.
    pub id: Option<String>,
}

impl Query {
    /// The statement and its hash. The suite and the id are the harness's to fill in, because the
    /// engine is handed a statement and has no idea what list it came from.
    #[must_use]
    pub fn new(sql: &str) -> Self {
        Self { sql: sql.to_string(), hash: hash(sql), suite: None, id: None }
    }
}

/// A hash of the statement text.
///
/// FNV-1a, sixty four bits, written out as hex. It is here rather than taken from somewhere else
/// because nothing in the workspace hashes text yet and this one has a job that asks almost nothing
/// of it: two runs of the same statement should land on the same string and two different
/// statements should not. It is not a checksum and nothing decides anything from it.
fn hash(sql: &str) -> String {
    let mut value: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in sql.as_bytes() {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{value:016x}")
}

/// The build that ran the query.
#[derive(Debug, Clone)]
pub struct Engine {
    /// The workspace version.
    pub version: String,
    /// The commit, when the build was told what it was.
    pub commit: Option<String>,
    /// `debug` or `release`, which is the first thing to check when a number looks wrong by a
    /// factor of ten.
    pub build: String,
}

impl Engine {
    /// This build.
    ///
    /// The commit comes from `RUDB_COMMIT` at compile time and is none in an ordinary local build,
    /// because a number that came from a working copy did not come from a commit and saying so is
    /// better than naming the commit the working copy was based on.
    #[must_use]
    pub fn here() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: option_env!("RUDB_COMMIT").map(str::to_string),
            build: if cfg!(debug_assertions) { "debug" } else { "release" }.to_string(),
        }
    }
}

/// The machine it ran on.
#[derive(Debug, Clone)]
pub struct Machine {
    /// What the fleet calls it.
    pub name: Option<String>,
    /// Cores the process was allowed to see.
    pub cores: u32,
    /// Physical memory in bytes.
    pub memory: Option<u64>,
    /// The operating system this was built for.
    pub os: String,
}

impl Machine {
    /// What this process can tell about where it is.
    ///
    /// The name and the memory size are none, and they stay none unless a harness fills them in.
    /// There is no portable way to ask for either, the machine the number will be compared against
    /// is the harness's business rather than the engine's, and a guessed host name in a published
    /// row is worse than an empty one.
    #[must_use]
    pub fn here() -> Self {
        let cores = std::thread::available_parallelism()
            .map_or(0, |cores| u32::try_from(cores.get()).unwrap_or(u32::MAX));
        Self { name: None, cores, memory: None, os: std::env::consts::OS.to_string() }
    }
}

/// What the query was allowed to use.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The memory limit in bytes, or none for no limit.
    pub memory_limit: Option<u64>,
    /// Threads the query was allowed.
    pub threads: u32,
}

/// How the execution ended.
///
/// A cancelled or failed run still produces a document, and every number in it is partial. The
/// warnings say so first, because a reader who missed that would compare a partial run against a
/// whole one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It ran to the end.
    Succeeded,
    /// Something asked it to stop and it did.
    Cancelled,
    /// It stopped on an error, which is the message.
    Failed(String),
}

impl Outcome {
    /// The word written in the document.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Cancelled => "cancelled",
            Self::Failed(_) => "failed",
        }
    }

    /// The error, when there was one.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Failed(message) => Some(message),
            _ => None,
        }
    }
}

/// Where the time went, in nanoseconds.
#[derive(Debug, Clone, Default)]
pub struct Timing {
    /// Text to an abstract syntax tree.
    pub parse_ns: u64,
    /// Syntax tree to a bound logical plan.
    pub bind_ns: u64,
    /// The optimizer passes.
    pub optimize_ns: u64,
    /// The physical plan and the operator tree.
    pub physical_ns: u64,
    /// Running it.
    pub execute_ns: u64,
    /// The whole call, which is more than the sum of the parts because the parts do not cover
    /// everything between them.
    pub total_ns: u64,
}

/// What the execution used.
#[derive(Debug, Clone, Default)]
pub struct Resource {
    /// CPU time across every thread. This is the axis that cannot be bought with threads.
    pub cpu_ns: u64,
    /// The high water mark of memory the engine accounted for.
    pub peak_bytes: u64,
    /// Bytes read at the point of the system call.
    pub bytes_read: u64,
    /// Bytes turned from a stored form into vectors.
    pub bytes_decoded: u64,
    /// Bytes written out to make room.
    pub bytes_spilled: u64,
    /// Bytes read back in again afterwards.
    pub bytes_read_back: u64,
    /// How many reads it took.
    pub io_requests: u64,
}

/// What was chosen at one seam.
#[derive(Debug, Clone)]
pub struct Strategy {
    /// The seam, such as `hash.table`.
    pub seam: String,
    /// The implementation that ran.
    pub chosen: String,
    /// How it came to be the one that ran, such as `default` or `pinned`.
    pub by: String,
    /// The paper or the measurement it comes from, when it cites one.
    pub provenance: Option<String>,
}

impl Strategy {
    /// A seam, what ran there and how it was picked.
    #[must_use]
    pub fn new(seam: &str, chosen: &str, by: &str) -> Self {
        Self {
            seam: seam.to_string(),
            chosen: chosen.to_string(),
            by: by.to_string(),
            provenance: None,
        }
    }
}

/// One pipeline.
#[derive(Debug, Clone)]
pub struct Pipeline {
    /// Its id, which the operators refer to.
    pub id: u32,
    /// How many copies of it ran.
    pub instances: u32,
    /// The pipelines that had to finish before this one could start.
    pub depends_on: Vec<u32>,
    /// Wall time from its first instance starting to its last one finishing.
    pub wall_ns: u64,
    /// CPU time across its instances.
    pub cpu_ns: u64,
    /// Where the waiting went.
    pub blocked: Blocked,
}

impl Pipeline {
    /// A pipeline with nothing measured yet.
    #[must_use]
    pub fn new(id: u32) -> Self {
        Self {
            id,
            instances: 1,
            depends_on: Vec::new(),
            wall_ns: 0,
            cpu_ns: 0,
            blocked: Blocked::default(),
        }
    }
}

/// Time spent waiting, split by the four reasons an operator can be blocked.
///
/// The four are the ones the push interface has, and there is no fifth. Time that cannot be put in
/// one of them is time the engine does not understand, which is what makes this worth splitting at
/// all.
#[derive(Debug, Clone, Default)]
pub struct Blocked {
    /// Waiting on a read.
    pub io_ns: u64,
    /// Waiting for memory to be available.
    pub memory_ns: u64,
    /// Waiting for a pipeline this one depends on.
    pub dependency_ns: u64,
    /// Waiting because whatever consumes this is not keeping up.
    pub downstream_ns: u64,
}

impl Blocked {
    /// All of it.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.io_ns
            .saturating_add(self.memory_ns)
            .saturating_add(self.dependency_ns)
            .saturating_add(self.downstream_ns)
    }

    /// The reason with the most time against it, and that time.
    #[must_use]
    pub fn largest(&self) -> (&'static str, u64) {
        [
            ("io", self.io_ns),
            ("memory", self.memory_ns),
            ("a dependency", self.dependency_ns),
            ("downstream", self.downstream_ns),
        ]
        .into_iter()
        .max_by_key(|(_, time)| *time)
        .unwrap_or(("io", 0))
    }
}

/// One operator.
#[derive(Debug, Clone)]
pub struct Operator {
    /// Its id, which a warning names.
    pub id: u32,
    /// The pipeline it ran in.
    pub pipeline: u32,
    /// What it is, such as `Scan` or `HashAggregate`.
    pub kind: String,
    /// The part of it worth printing, such as the table or the keys.
    pub detail: Option<String>,
    /// Rows it was handed.
    pub rows_in: u64,
    /// Rows it produced.
    pub rows_out: u64,
    /// What the optimizer thought it would produce, when it thought anything.
    pub estimated_rows: Option<u64>,
    /// Wall time inside it.
    pub wall_ns: u64,
    /// CPU time inside it, across every instance.
    pub cpu_ns: u64,
    /// Bytes it read.
    pub bytes_read: u64,
    /// Bytes it decoded.
    pub bytes_decoded: u64,
    /// Bytes it spilled.
    pub bytes_spilled: u64,
    /// What it held.
    pub memory: Memory,
    /// Whether what ran was the reference implementation rather than a fast one.
    pub reference_impl: bool,
}

impl Operator {
    /// An operator with nothing measured yet.
    #[must_use]
    pub fn new(id: u32, pipeline: u32, kind: &str) -> Self {
        Self {
            id,
            pipeline,
            kind: kind.to_string(),
            detail: None,
            rows_in: 0,
            rows_out: 0,
            estimated_rows: None,
            wall_ns: 0,
            cpu_ns: 0,
            bytes_read: 0,
            bytes_decoded: 0,
            bytes_spilled: 0,
            memory: Memory::default(),
            reference_impl: false,
        }
    }

    /// How it appears in a warning, which is the id and the kind because the id alone is a number
    /// nobody can place.
    #[must_use]
    pub fn named(&self) -> String {
        format!("operator {} ({})", self.id, self.kind)
    }
}

/// What one operator held.
#[derive(Debug, Clone, Default)]
pub struct Memory {
    /// What it has reserved from the budget right now, which is zero by the time a query ends.
    pub reserved: u64,
    /// The most it ever held.
    pub high_water: u64,
}

#[cfg(test)]
mod tests {
    use super::{Document, Engine, Machine, Operator, Outcome, Pipeline, Strategy};

    /// A document with every part of it filled in, which is what the golden file holds.
    fn sample() -> Document {
        let mut metrics = Document::new("select ClientIP, count(*) from hits group by 1");
        metrics.query.suite = Some("clickbench".to_string());
        metrics.query.id = Some("q32".to_string());
        metrics.engine = Engine {
            version: "0.3.0".to_string(),
            commit: Some("ab1510c".to_string()),
            build: "release".to_string(),
        };
        metrics.machine = Machine {
            name: Some("server3".to_string()),
            cores: 8,
            memory: Some(24_696_061_952),
            os: "linux".to_string(),
        };
        metrics.settings.memory_limit = Some(1_073_741_824);
        metrics.settings.threads = 8;
        metrics.timing.parse_ns = 41_000;
        metrics.timing.bind_ns = 88_000;
        metrics.timing.optimize_ns = 310_000;
        metrics.timing.physical_ns = 44_000;
        metrics.timing.execute_ns = 1_323_000_000;
        metrics.timing.total_ns = 1_323_483_000;
        metrics.resource.cpu_ns = 9_880_000_000;
        metrics.resource.peak_bytes = 894_000_000;
        metrics.resource.bytes_read = 1_420_000_000;
        metrics.resource.bytes_decoded = 210_000_000;
        metrics.resource.io_requests = 1_180;

        let mut table = Strategy::new("hash.table", "unchained", "default");
        table.provenance = Some("Birler et al., DaMoN 2024".to_string());
        metrics.strategies.push(table);
        metrics.strategies.push(Strategy::new("topk", "heavy-hitter-two-pass", "pinned"));

        let mut scan = Pipeline::new(0);
        scan.instances = 8;
        scan.wall_ns = 980_000_000;
        scan.cpu_ns = 7_600_000_000;
        scan.blocked.io_ns = 120_000_000;
        scan.blocked.downstream_ns = 3_000_000;
        let mut top = Pipeline::new(1);
        top.depends_on.push(0);
        top.wall_ns = 343_000_000;
        top.cpu_ns = 2_280_000_000;
        metrics.pipelines.extend([scan, top]);

        let mut read = Operator::new(3, 0, "Scan");
        read.detail = Some("hits".to_string());
        read.rows_out = 99_997_497;
        read.estimated_rows = Some(99_997_497);
        read.wall_ns = 620_000_000;
        read.cpu_ns = 4_800_000_000;
        read.bytes_read = 1_420_000_000;
        read.bytes_decoded = 210_000_000;
        let mut group = Operator::new(5, 0, "HashAggregate");
        group.detail = Some("ClientIP".to_string());
        group.rows_in = 99_997_497;
        group.rows_out = 41_983_110;
        group.estimated_rows = Some(2_400_000);
        group.wall_ns = 360_000_000;
        group.cpu_ns = 2_800_000_000;
        group.memory.high_water = 894_000_000;
        let mut sort = Operator::new(7, 1, "Sort");
        sort.rows_in = 41_983_110;
        sort.rows_out = 10;
        sort.wall_ns = 343_000_000;
        sort.cpu_ns = 2_280_000_000;
        sort.bytes_spilled = 12_000_000;
        sort.reference_impl = true;
        metrics.operators.extend([read, group, sort]);
        metrics
    }

    /// The compatibility test the crate documentation promises.
    ///
    /// `schema/1.json` is a document written by this code at the commit that added it, and it is in
    /// the repository so that a change to the shape shows up as a diff on a file somebody has to
    /// look at rather than as a surprise in a harness three repositories away. Adding a field
    /// changes this file and that is fine. Renaming one is the migration the schema number is for.
    #[test]
    fn the_document_renders_as_schema_one() {
        assert_eq!(sample().render(), include_str!("../schema/1.json"));
    }

    /// The same document, on one line, with the same keys in the same order.
    ///
    /// Whitespace is the only difference, which is what makes a file of one document per line
    /// readable a record at a time without a second schema to describe it.
    #[test]
    fn the_one_line_form_is_the_indented_one_with_the_whitespace_taken_out() {
        let document = sample();
        let line = document.one_line();
        assert!(!line.contains('\n'), "{line}");
        assert!(line.starts_with("{\"schema\":1,"), "{line}");
        assert!(line.ends_with('}'), "{line}");
        let flattened: String =
            document.render().chars().filter(|character| !character.is_whitespace()).collect();
        let compared: String =
            line.chars().filter(|character| !character.is_whitespace()).collect();
        assert_eq!(flattened, compared);
    }

    #[test]
    fn a_failed_query_still_has_a_document() {
        let mut metrics = Document::new("select 1 / 0");
        metrics.outcome = Outcome::Failed("division by \"zero\"".to_string());
        metrics.timing.parse_ns = 900;
        let rendered = metrics.render();
        assert!(rendered.contains("\"state\": \"failed\""), "{rendered}");
        assert!(rendered.contains("\"message\": \"division by \\\"zero\\\"\""), "{rendered}");
        assert!(
            rendered.contains("the query failed, so every number here is partial"),
            "{rendered}"
        );
    }

    #[test]
    fn the_same_statement_hashes_the_same_way_and_a_different_one_does_not() {
        let one = Document::new("select 1");
        let same = Document::new("select 1");
        let other = Document::new("select 2");
        assert_eq!(one.query.hash, same.query.hash);
        assert_ne!(one.query.hash, other.query.hash);
        assert_eq!(one.query.hash.len(), 16);
    }
}
