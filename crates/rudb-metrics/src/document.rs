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

use rudb_common::stat::{Class, Classes, Provenance};
use rudb_common::{Spent, Tally};

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
    /// How much the planner knew, one decision per operator.
    ///
    /// The class histogram of `spec/stats/09-measurement.md` section 9.5. A harness sums these over
    /// a suite and gets the fraction of the planner's cardinality decisions that rested on a
    /// counted number, a bounded one, a guess or nothing at all. That fraction is the direct
    /// measurement of whether the statistics layer is doing its job, and it is the number the G
    /// series is trying to move.
    pub estimates: Classes,
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
            estimates: Classes::new(),
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
            out.count("rewrite_ns", self.timing.rewrite_ns);
            out.count("physical_ns", self.timing.physical_ns);
            out.count("execute_ns", self.timing.execute_ns);
            out.count("result_ns", self.timing.result_ns);
            out.count("total_ns", self.timing.total_ns);
        });
        // Worked out from the operators below rather than stored, and written here so a reader who
        // wants the one line does not have to know which stage goes where. See [`crate::Split`].
        out.key("split");
        out.object(|out| {
            for (name, nanos) in self.split().parts() {
                out.count(&format!("{name}_ns"), nanos);
            }
        });
        out.key("resource");
        out.object(|out| {
            out.count("cpu_ns", self.resource.cpu_ns);
            out.count("build_cpu_ns", self.resource.build_cpu_ns);
            out.count("peak_bytes", self.resource.peak_bytes);
            out.count("bytes_read", self.resource.bytes_read);
            out.count("bytes_decoded", self.resource.bytes_decoded);
            out.count("bytes_spilled", self.resource.bytes_spilled);
            out.count("bytes_read_back", self.resource.bytes_read_back);
            out.count("io_requests", self.resource.io_requests);
        });
        out.key("estimates");
        out.object(|out| {
            out.count("exact", self.estimates.exact());
            out.count("certified", self.estimates.certified());
            out.count("estimated", self.estimates.estimated());
            out.count("unknown", self.estimates.unknown());
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
                    out.count("slowest_ns", pipeline.slowest_ns);
                    out.count("slowest_cpu_ns", pipeline.slowest_cpu_ns);
                    out.count("finalize_ns", pipeline.finalize_ns);
                    out.count("stagger_ns", pipeline.stagger_ns);
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
                    out.maybe_count("parent", operator.parent.map(u64::from));
                    out.words("kind", &operator.kind);
                    out.maybe_words("detail", operator.detail.as_deref());
                    out.count("rows_in", operator.rows_in);
                    out.count("rows_out", operator.rows_out);
                    out.maybe_count("estimated_rows", operator.estimated_rows);
                    out.maybe_words(
                        "estimate_class",
                        operator.estimate_class.map(|class| class.to_string()).as_deref(),
                    );
                    out.maybe_words(
                        "estimate_provenance",
                        operator.estimate_provenance.map(|from| from.to_string()).as_deref(),
                    );
                    out.count("wall_ns", operator.wall_ns);
                    out.count("cpu_ns", operator.cpu_ns);
                    out.count("bytes_read", operator.bytes_read);
                    out.count("bytes_decoded", operator.bytes_decoded);
                    out.count("bytes_spilled", operator.bytes_spilled);
                    // Only a scan has parts, and a row of two zeroes under a hash join is two more
                    // keys a reader has to look at to find out they say nothing.
                    if operator.parts_read != 0 || operator.parts_pruned != 0 {
                        out.count("parts_read", operator.parts_read);
                        out.count("parts_pruned", operator.parts_pruned);
                    }
                    out.key("fallbacks");
                    out.object(|out| {
                        out.count("total", operator.fallbacks.total());
                        for (cause, times) in operator.fallbacks.taken() {
                            out.count(cause.name(), times);
                        }
                    });
                    if !operator.stages.is_empty() {
                        out.key("stages");
                        out.object(|out| {
                            for (stage, nanos, bytes) in operator.stages.taken() {
                                out.key(stage.name());
                                out.object(|out| {
                                    out.count("wall_ns", nanos);
                                    out.count("bytes", bytes);
                                });
                            }
                        });
                    }
                    out.key("memory");
                    out.object(|out| {
                        out.count("reserved", operator.memory.reserved);
                        out.count("high_water", operator.memory.high_water);
                    });
                    if !operator.implementations.is_empty() {
                        out.key("implementations");
                        out.array(|out| {
                            for chosen in &operator.implementations {
                                out.item();
                                out.object(|out| {
                                    out.words("seam", &chosen.seam);
                                    out.words("name", &chosen.name);
                                    out.flag("is_reference", chosen.is_reference);
                                });
                            }
                        });
                    }
                    out.flag("reference_impl", operator.reference_impl);
                    // Only a join has one, and the rest of the plan is most of the plan, so this
                    // is a key that is there when it says something and absent when it does not.
                    if let Some(joined) = &operator.joined {
                        out.key("join");
                        out.object(|out| {
                            out.words("algorithm", joined.algorithm.name());
                            out.count("build_rows", joined.build_rows);
                            out.count("build_bytes", joined.build_bytes);
                            out.key("declined");
                            out.array(|out| {
                                for declined in &joined.declined {
                                    out.item();
                                    out.object(|out| {
                                        out.words("algorithm", declined.algorithm.name());
                                        out.words("reason", &declined.reason);
                                    });
                                }
                            });
                        });
                    }
                    // Only a scan under a reduced join has one, for the reason a join is the only
                    // operator with the key above.
                    if let Some(reduced) = &operator.reduced {
                        out.key("reduced");
                        out.object(|out| {
                            out.count("kept", reduced.kept);
                            out.count("rows", reduced.rows);
                            out.flag("stopped", reduced.stopped);
                            out.flag("by_key", reduced.by_key);
                        });
                    }
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
    /// The part of `optimize_ns` that went on the rewrites, which is every pass in front of join
    /// ordering and the lowering of dependent joins before them.
    ///
    /// A part of `optimize_ns` rather than a phase of its own next to it, because a reader of an
    /// older document takes `optimize_ns` to be the whole optimizer and changing that would be a
    /// schema change. The optimizer proper is `optimize_ns` minus this, and `rudb_statement_metrics()`
    /// prints the two apart.
    pub rewrite_ns: u64,
    /// The physical plan and the operator tree.
    pub physical_ns: u64,
    /// Running it.
    pub execute_ns: u64,
    /// The part of `execute_ns` that went on turning the answer into flat columns for the caller.
    ///
    /// Summed over the threads that did it, like an operator's time. The root that does it is not
    /// an operator, so without this the last copy a query makes would be in no row at all.
    pub result_ns: u64,
    /// The whole call, which is the five phases above added up.
    ///
    /// It used to be `physical_ns` plus `execute_ns`, because nothing above the executor was on a
    /// clock, so a query that spent most of itself in the optimizer reported most of itself as
    /// nothing at all.
    pub total_ns: u64,
}

/// What the execution used.
#[derive(Debug, Clone, Default)]
pub struct Resource {
    /// CPU time across every thread. This is the axis that cannot be bought with threads.
    pub cpu_ns: u64,
    /// The part of `cpu_ns` that went on building the operator tree rather than running it.
    ///
    /// Split out because no operator can ever account for it. Opening a file, reading its schema,
    /// deciding which implementation runs at each seam and allocating the tree all happen before
    /// there is an operator to charge, so a check that compared the operators against `cpu_ns`
    /// would read the whole of this as time that went missing. `cpu_ns` minus this is the span the
    /// pipelines are inside of, and that is what the cross check in `rudb-bench` compares against.
    pub build_cpu_ns: u64,
    /// The high water mark of memory the engine accounted for.
    ///
    /// "Accounted for" is the whole of the claim and is narrower than it reads. What is counted is
    /// what an operator registers, which in practice is hash aggregate group tables and sort
    /// buffers, and what is not counted is string materialisation, decode buffers and the reader's
    /// page cache. So this is a lower bound on the process and on some query shapes it is a very
    /// loose one: `spec/storage-v3/21` measures ClickBench q23 at 712 MiB resident while this field
    /// reports 0.4 MiB for the same statement, and the suite's peak resident set exceeds the
    /// maximum of this field across the run by 774 MiB.
    ///
    /// Read it as "memory the operators asked for" and never as "memory the process used". Document
    /// 19 of that series built an argument about the resource half of the project's target on this
    /// field and named the wrong four queries, which is why the caveat is here rather than only
    /// there.
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
    /// The wall of the longest single instance, which is the one the rest waited for.
    ///
    /// What sits between this and `wall_ns` is what starting and joining the threads cost. What
    /// sits between this and `cpu_ns` divided by `instances` is how unevenly the work was split,
    /// less whatever `stagger_ns` accounts for. Both of those used to be one unnamed number that
    /// had to be got at by subtracting the operators from the pipeline, and on ClickBench 39 that
    /// number is more than half the query.
    pub slowest_ns: u64,
    /// The CPU of that same instance, which says whether it was working or waiting.
    ///
    /// An instance twice as long as the average either had twice the work or spent half its time
    /// waiting for a lock, and the two want opposite fixes. Near `slowest_ns` is work and far below
    /// it is waiting.
    pub slowest_cpu_ns: u64,
    /// The wall of the sink's finalize, which runs once on one thread after every instance is done.
    ///
    /// A grouped aggregate does most of its work here and starts its own threads to do it. This is
    /// the wall of the whole thing, undivided, which is the number that matters when the question
    /// is what the query waited for.
    pub finalize_ns: u64,
    /// How long after the first instance started the last one did.
    ///
    /// The instances do not start together, they start as the dispatching thread wakes them, and
    /// an instance woken last finishes last even when every instance is handed identical work. So
    /// this is the part of the gap between `slowest_ns` and the average instance that is not
    /// imbalance, and the two want opposite fixes: imbalance wants the work cut more finely and
    /// stagger wants the waking made cheaper. Reading the gap as imbalance without this number is
    /// how a scan came to be blamed for it once.
    pub stagger_ns: u64,
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
            slowest_ns: 0,
            slowest_cpu_ns: 0,
            finalize_ns: 0,
            stagger_ns: 0,
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
    /// The operator its rows went into, and nothing for the one that produced the answer.
    ///
    /// The only edge in this list. Everything else here is flat with a pipeline id on it, and a
    /// pipeline is not enough to walk the tree: a pipeline holds several operators in a line and
    /// says nothing about which of them fed which. Without this a reader can add up the rows a
    /// query moved and cannot check that any of them add up, since the check is whether an
    /// operator's input equals what its children produced and there is no other way to know what
    /// its children were.
    ///
    /// It is the operator the rows actually went to rather than the parent in the plan, and those
    /// differ in one place: a node with two inputs is two operators, and the side that has to
    /// finish first feeds the operator that holds it, which then feeds the join.
    pub parent: Option<u32>,
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
    /// How much of that was knowledge, when it thought anything.
    ///
    /// `None` is the same answer [`Operator::estimated_rows`] gives as `None`, which is that nobody
    /// had a number here. The two always agree, and they are two fields rather than one because a
    /// reader that wants the count should not have to parse a word to get it.
    pub estimate_class: Option<Class>,
    /// Where that number came from, when there was one.
    ///
    /// A third field rather than a word inside the class, because an exact count out of the catalog
    /// and an exact join cardinality out of a link header are different kinds of exact and a reader
    /// chasing a bad plan has to be able to tell them apart. `spec/stats/02-the-catalogue.md`
    /// section 2.1.1. `default` is the one to search for, since it means nobody had a number at
    /// that operator at all.
    pub estimate_provenance: Option<Provenance>,
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
    /// Parts of the table it read, for an operator that reads one.
    ///
    /// A part is whatever the storage prunes at, which is a chunk of 1024 rows for a table in
    /// memory and a part of a stripe for a native file. Zero for everything that is not a scan.
    pub parts_read: u64,
    /// Parts the statistics ruled out, so they were never read, decoded or filtered.
    ///
    /// This is the number that says whether the physical order of the table is doing any work.
    /// A predicate on a column the rows are not ordered by prunes nothing, however selective it
    /// is, and the only way to tell that apart from a predicate nothing matches is to count what
    /// was skipped. `spec/perf/` and the TPC-H work both turn on this number and it was private
    /// to the scan until now.
    pub parts_pruned: u64,
    /// How many times something inside it took a path written to be correct rather than fast.
    ///
    /// This is the one number in the row that is a work list rather than a measurement. Every count
    /// in it is a kernel that met a pair of forms nobody has written a loop for yet, or a column
    /// that was copied out of its compact form because whoever was handed it could not read it. Both
    /// are the difference between what the engine does and what the data plane was designed to do,
    /// and F7 is meant to be this list sorted by cost rather than a guess about where to look.
    pub fallbacks: Tally,
    /// Where the time went inside a read, when this operator does any reading.
    ///
    /// Written only by a scan, and missing from the document entirely for every operator that does
    /// not read, because a row of five zeroes under a filter says nothing and costs a reader the
    /// time it takes to work out that it says nothing. The stages do not have to add up to the
    /// operator's own time: what is left over is the scan's own bookkeeping, and how much of it
    /// there is is worth knowing on its own.
    pub stages: Spent,
    /// What it held.
    pub memory: Memory,
    /// What it picked at each of the seams it sits on that has more than one thing to pick from.
    ///
    /// Empty for an operator that sits on no seam, such as a limit, and empty for one whose seams
    /// have no registry yet, which at F1 is most of them. Empty therefore means there was nothing
    /// to choose and not that nothing ran.
    pub implementations: Vec<Implementation>,
    /// Whether everything this operator chose was a reference implementation.
    ///
    /// [`Counters::snapshot`](crate::Counters::snapshot) sets this true when
    /// [`Operator::implementations`] is empty, on purpose and for the same reason `EXPLAIN` puts
    /// the marker on a line with no registered seam under it. Nothing was chosen, so what ran is
    /// the one implementation there is, and that one is the obvious correct one.
    ///
    /// A row built by hand rather than measured starts false, because a document assembled in a
    /// test has made no claim either way and a warning about it would be a warning about the test.
    pub reference_impl: bool,
    /// What a join did, for an operator that is one, and nothing for everything else.
    ///
    /// A join is the one operator in the engine with two inputs and one row in this list, so the
    /// row's own [`Operator::rows_in`] is the driving side alone and the gathered side is
    /// unaccounted for without this. See [`Joined`].
    pub joined: Option<Joined>,
    /// What a join's exact reduction left this scan to read, for a scan under one.
    ///
    /// Nothing for every other operator, and nothing for a scan whose join had no stored link to
    /// reduce it with. See [`Reduced`].
    pub reduced: Option<Reduced>,
}

/// What pushing a join's build side through a stored link left the scan under it.
///
/// spec/graph/05-execution.md section 5.4. The number worth having is `kept` against `rows`,
/// because it is the one that says whether the reduction paid for itself, and a reduction that
/// stopped early is written down as one because its `kept` is every row and a reader would
/// otherwise take that for a set that happened to hold everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reduced {
    /// Rows of the table the reduction said can match.
    pub kept: u64,
    /// Rows of the table.
    pub rows: u64,
    /// Whether the push gave up after a third of the table because it had removed nothing.
    pub stopped: bool,
    /// Whether the build side's keys were tested against a bitmap over the parent's key range
    /// rather than pushed through a link, which is what a join over a relationship whose link is not
    /// in the file gets. Then `kept` and `rows` count the parent's keys, because the rows of the
    /// table are only tested as they are read.
    pub by_key: bool,
}

/// How a join found the rows one row matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// A table over the gathered side, read once per driving row.
    Hash,
    /// Every driving row against every gathered row, which is the answer for a condition with no
    /// equality in it.
    Loop,
    /// The nth row of one side with the nth of the other, which is not a search at all.
    Positional,
}

impl Algorithm {
    /// What it is called in the document.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hash => "hash",
            Self::Loop => "nested loop",
            Self::Positional => "positional",
        }
    }
}

/// What a join did, beside the rows and the time every operator reports.
///
/// Reported once, by whichever instance of the operator built the gathered side, and reported as
/// one record rather than a number at a time because the algorithm, the reasons and the two build
/// numbers are all settled within a few lines of each other. A record assembled from four calls is
/// a record that can be found half written.
///
/// What is not here is the driving side and the answer. Those are [`Operator::rows_in`] and
/// [`Operator::rows_out`] on the same row, they are counted by the shim around every operator
/// rather than by the join, and a second copy of them here would be a second copy that can
/// disagree with the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    /// How the rows were matched.
    pub algorithm: Algorithm,
    /// Rows of the gathered side, which is the side the table is built from.
    ///
    /// Zero says the gathered side was empty, which is worth telling apart from a join that
    /// produced nothing because nothing matched.
    pub build_rows: u64,
    /// What the table and the copy of the side under it are charged.
    ///
    /// Not the whole of what the gathered side costs. The chunks it arrived in are charged to the
    /// operator that gathered them, which is a row of its own in this list, and charging them
    /// again here would say the join holds twice what it holds.
    pub build_bytes: u64,
    /// The algorithms this join did not use, each with the reason it did not.
    ///
    /// The point of recording a road not taken is that a join running the wrong algorithm is the
    /// most expensive single thing a plan can do, and the number in front of somebody reading this
    /// says only what happened. Empty where there was nothing else in the running.
    pub declined: Vec<Declined>,
}

/// One algorithm a join did not use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declined {
    /// What was not used, in the same words [`Algorithm::name`] uses.
    pub algorithm: Algorithm,
    /// Why not, in a sentence.
    pub reason: String,
}

impl Declined {
    /// One road not taken.
    #[must_use]
    pub fn new(algorithm: Algorithm, reason: &str) -> Self {
        Self { algorithm, reason: reason.to_string() }
    }
}

/// What one operator picked at one seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementation {
    /// The seam, in its dotted name such as `chunk.compaction`.
    pub seam: String,
    /// What is running there.
    pub name: String,
    /// Whether that is the reference implementation.
    pub is_reference: bool,
}

impl Operator {
    /// An operator with nothing measured yet.
    #[must_use]
    pub fn new(id: u32, pipeline: u32, kind: &str) -> Self {
        Self {
            id,
            pipeline,
            parent: None,
            kind: kind.to_string(),
            detail: None,
            rows_in: 0,
            rows_out: 0,
            estimated_rows: None,
            estimate_class: None,
            estimate_provenance: None,
            wall_ns: 0,
            cpu_ns: 0,
            bytes_read: 0,
            bytes_decoded: 0,
            bytes_spilled: 0,
            parts_read: 0,
            parts_pruned: 0,
            fallbacks: Tally::none(),
            stages: Spent::none(),
            memory: Memory::default(),
            implementations: Vec::new(),
            reference_impl: false,
            joined: None,
            reduced: None,
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
    use rudb_common::stat::{Class, Direction, Provenance};
    use rudb_common::{Cause, Spent, Stage, Tally};

    use super::{
        Algorithm, Declined, Document, Engine, Implementation, Joined, Machine, Operator, Outcome,
        Pipeline, Strategy,
    };

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
        metrics.timing.rewrite_ns = 120_000;
        metrics.timing.physical_ns = 44_000;
        metrics.timing.execute_ns = 1_323_000_000;
        metrics.timing.result_ns = 2_000_000;
        metrics.timing.total_ns = 1_323_483_000;
        metrics.resource.cpu_ns = 9_880_000_000;
        metrics.resource.build_cpu_ns = 44_000;
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
        scan.slowest_ns = 940_000_000;
        scan.slowest_cpu_ns = 912_000_000;
        scan.finalize_ns = 31_000_000;
        scan.stagger_ns = 7_000_000;
        scan.blocked.io_ns = 120_000_000;
        scan.blocked.downstream_ns = 3_000_000;
        let mut top = Pipeline::new(1);
        top.depends_on.push(0);
        top.wall_ns = 343_000_000;
        top.cpu_ns = 2_280_000_000;
        top.slowest_ns = 343_000_000;
        top.slowest_cpu_ns = 180_000_000;
        metrics.pipelines.extend([scan, top]);

        let mut read = Operator::new(3, 0, "Scan");
        read.parent = Some(5);
        read.detail = Some("hits".to_string());
        read.rows_out = 99_997_497;
        read.estimated_rows = Some(99_997_497);
        read.estimate_class = Some(Class::Exact);
        read.estimate_provenance = Some(Provenance::RowCount);
        read.wall_ns = 620_000_000;
        read.cpu_ns = 4_800_000_000;
        read.bytes_read = 1_420_000_000;
        read.bytes_decoded = 210_000_000;
        read.stages.add(Spent::of(Stage::Read, 120_000_000, 1_420_000_000));
        read.stages.add(Spent::of(Stage::Decompress, 210_000_000, 210_000_000));
        read.stages.add(Spent::of(Stage::Decode, 240_000_000, 210_000_000));
        read.stages.add(Spent::of(Stage::Dictionary, 9_000_000, 1_400_000));
        read.stages.add(Spent::of(Stage::Assemble, 22_000_000, 0));
        let mut group = Operator::new(5, 0, "HashAggregate");
        group.parent = Some(6);
        group.detail = Some("ClientIP".to_string());
        group.rows_in = 99_997_497;
        group.rows_out = 41_983_110;
        group.estimated_rows = Some(2_400_000);
        group.estimate_class = Some(Class::Estimated);
        group.estimate_provenance = Some(Provenance::Default);
        group.wall_ns = 360_000_000;
        group.cpu_ns = 2_800_000_000;
        group.memory.high_water = 894_000_000;
        group.fallbacks.add(Tally::of(Cause::Flatten, 48_827));
        group.fallbacks.add(Tally::of(Cause::Aggregate, 48_827));
        group.implementations.push(Implementation {
            seam: "hash.table".to_string(),
            name: "unchained".to_string(),
            is_reference: false,
        });
        let mut probe = Operator::new(6, 0, "Probe");
        probe.parent = Some(7);
        probe.detail = Some("hits.ClientIP = banned.ClientIP".to_string());
        probe.rows_in = 41_983_110;
        probe.rows_out = 41_983_110;
        probe.estimated_rows = Some(41_983_110);
        probe.estimate_class = Some(Class::Estimated);
        probe.estimate_provenance = Some(Provenance::Default);
        probe.wall_ns = 210_000_000;
        probe.cpu_ns = 1_400_000_000;
        probe.joined = Some(Joined {
            algorithm: Algorithm::Hash,
            build_rows: 4096,
            build_bytes: 262_144,
            declined: vec![Declined::new(
                Algorithm::Loop,
                "the condition holds an equality, so a driving row's matches are one lookup",
            )],
        });
        let mut sort = Operator::new(7, 1, "Sort");
        sort.rows_in = 41_983_110;
        sort.rows_out = 10;
        sort.estimated_rows = Some(10);
        sort.estimate_class = Some(Class::Certified { bound: 1.0, direction: Direction::AtMost });
        sort.estimate_provenance = Some(Provenance::Default);
        sort.wall_ns = 343_000_000;
        sort.cpu_ns = 2_280_000_000;
        sort.bytes_spilled = 12_000_000;
        sort.fallbacks.add(Tally::of(Cause::Compare, 12));
        sort.implementations.push(Implementation {
            seam: "sort".to_string(),
            name: "merge".to_string(),
            is_reference: true,
        });
        sort.reference_impl = true;
        for operator in [&read, &group, &probe, &sort] {
            metrics.estimates.record_class(operator.estimate_class);
        }
        metrics.operators.extend([read, group, probe, sort]);
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
