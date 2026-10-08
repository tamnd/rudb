//! What a PostgreSQL `EXPLAIN` was asked for.
//!
//! The binder reads the option list into [`Options`] and the optimizer prints the plan from it. The
//! two are apart because the binder is where an option is checked and the optimizer is where the
//! estimates are, and the plan is the only thing both of them see.

/// The format of the output, which is the `FORMAT` option.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    /// One row for each line, indented by the depth of the node.
    #[default]
    Text,
    /// One row of XML in the namespace `http://www.postgresql.org/2009/explain`.
    Xml,
    /// One row of JSON.
    Json,
    /// One row of YAML.
    Yaml,
}

/// What `SERIALIZE` asks for, which is how the result is turned into the bytes a client is sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Serialize {
    /// The rows are not serialized, which is the default.
    #[default]
    None,
    /// The rows are serialized in the text format.
    Text,
    /// The rows are serialized in the binary format.
    Binary,
}

/// The options of one `EXPLAIN`, after the defaults that depend on `ANALYZE` are applied.
///
/// The fields are the fields of PostgreSQL's `ExplainState` that an option sets, with the same
/// names, so a reader of `explain.c` finds each one here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "one flag for each option of the statement")]
pub struct Options {
    /// The output format.
    pub format: Format,
    /// Run the query and print what happened.
    pub analyze: bool,
    /// Print the output columns of each node and qualify the names.
    pub verbose: bool,
    /// Print the estimated costs, rows and width of each node.
    pub costs: bool,
    /// Print the buffer counts.
    pub buffers: bool,
    /// Print the WAL counts.
    pub wal: bool,
    /// Print the settings that differ from their defaults.
    pub settings: bool,
    /// Plan the query with its parameters left unknown.
    pub generic: bool,
    /// Print the time each node took.
    pub timing: bool,
    /// Print the planning and execution times.
    pub summary: bool,
    /// Print the memory the planner used.
    pub memory: bool,
    /// Serialize the result and print what that cost.
    pub serialize: Serialize,
    /// Print the I/O counts.
    pub io: bool,
}

impl Default for Options {
    /// What `EXPLAIN` with no options means: the plan with its costs, as text.
    fn default() -> Self {
        Self {
            format: Format::Text,
            analyze: false,
            verbose: false,
            costs: true,
            buffers: false,
            wal: false,
            settings: false,
            generic: false,
            timing: false,
            summary: false,
            memory: false,
            serialize: Serialize::None,
            io: false,
        }
    }
}
