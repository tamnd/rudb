//! Pipelines, per `spec/compiler/05-pipelines-and-state.md`.
//!
//! [`split`] cuts a [`Rel`] tree at its breakers and returns the stages in the order they run. A
//! [`Pipeline`] is a source, the filters that apply to it and a sink. The filters and projections
//! between a source and the next breaker are folded into it by substitution, so a filter above a
//! projection becomes a filter over the source's columns and the sink's expressions read the source
//! directly. Nothing between two breakers is left as an operator of its own, which is the point of
//! compiling a pipeline: one loop over the rows of a morsel, with no chunk in between.
//!
//! The breakers are the aggregate, which is a sink, and the sort, top N, limit and fetch, which in
//! C1 are stages of their own over the rows the pipeline before them produced. They are few rows
//! or cheap work in ClickBench, and a compiled top N is part of C3.

use std::fmt;

use rudb_common::Value;
use rudb_plan::NodeRef;
use rudb_qc_plan::{Aggregate, Column, Expr, Key, Kind, Rel};

/// Where a pipeline's rows come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A base table, by its `Get` node.
    Scan {
        /// The node in the original plan.
        node: NodeRef,
        /// The table, for messages.
        table: String,
        /// The columns.
        columns: Vec<Column>,
    },
    /// Literal rows.
    Values {
        /// The rows.
        rows: Vec<Vec<Value>>,
        /// The columns.
        columns: Vec<Column>,
    },
    /// The rows an earlier stage produced.
    Stage {
        /// The stage.
        stage: usize,
        /// Its columns.
        columns: Vec<Column>,
    },
}

impl Source {
    /// The columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        match self {
            Source::Scan { columns, .. }
            | Source::Values { columns, .. }
            | Source::Stage { columns, .. } => columns,
        }
    }
}

/// Where a pipeline's rows go.
#[derive(Clone, Debug, PartialEq)]
pub enum Sink {
    /// Rows out, one per input row that passed the filters.
    Result {
        /// The output columns, over the source's.
        exprs: Vec<Expr>,
        /// Their names and types.
        columns: Vec<Column>,
    },
    /// A hash aggregate. The output is the groups and then the aggregates.
    Aggregate {
        /// The group expressions, over the source's columns.
        groups: Vec<Expr>,
        /// The aggregate calls, over the source's columns.
        aggregates: Vec<Aggregate>,
        /// Their names and types.
        columns: Vec<Column>,
    },
}

/// One compiled loop.
#[derive(Clone, Debug, PartialEq)]
pub struct Pipeline {
    /// Where the rows come from.
    pub source: Source,
    /// The predicates a row has to pass, in order, over the source's columns.
    pub filters: Vec<Expr>,
    /// Where the rows go.
    pub sink: Sink,
}

/// One step of a query.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// A compiled pipeline.
    Pipeline(Pipeline),
    /// All of an earlier stage's rows in order.
    Sort {
        /// The stage.
        input: usize,
        /// The keys, over its columns.
        keys: Vec<Key>,
        /// Its columns.
        columns: Vec<Column>,
    },
    /// The first rows of an earlier stage in order.
    TopN {
        /// The stage.
        input: usize,
        /// The keys, over its columns.
        keys: Vec<Key>,
        /// How many rows.
        count: u64,
        /// How many to skip first.
        offset: u64,
        /// Its columns.
        columns: Vec<Column>,
    },
    /// Some of an earlier stage's rows.
    Limit {
        /// The stage.
        input: usize,
        /// How many rows, all when `None`.
        count: Option<u64>,
        /// How many to skip first.
        offset: u64,
        /// Its columns.
        columns: Vec<Column>,
    },
    /// Whole table rows read back by the ordinals an earlier stage produced.
    Fetch {
        /// The stage.
        input: usize,
        /// The `TableFetch` node in the plan.
        node: NodeRef,
        /// The ordinal, over the stage's columns.
        row: Expr,
        /// The output columns.
        columns: Vec<Column>,
    },
}

impl Stage {
    /// The columns the stage produces.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        match self {
            Stage::Pipeline(p) => match &p.sink {
                Sink::Result { columns, .. } | Sink::Aggregate { columns, .. } => columns,
            },
            Stage::Sort { columns, .. }
            | Stage::TopN { columns, .. }
            | Stage::Limit { columns, .. }
            | Stage::Fetch { columns, .. } => columns,
        }
    }
}

/// A query as stages. Each stage reads only stages before it, and the last one is the answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Graph {
    /// The stages in the order they run.
    pub stages: Vec<Stage>,
}

impl Graph {
    /// The columns of the answer.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        self.stages.last().map_or(&[], Stage::columns)
    }

    /// The pipelines, with their stage numbers.
    pub fn pipelines(&self) -> impl Iterator<Item = (usize, &Pipeline)> {
        self.stages.iter().enumerate().filter_map(|(i, s)| match s {
            Stage::Pipeline(p) => Some((i, p)),
            _ => None,
        })
    }
}

/// Cuts a physical plan into stages.
#[must_use]
pub fn split(rel: &Rel) -> Graph {
    let mut g = Graph { stages: Vec::new() };
    let open = g.open(rel);
    g.close(open);
    g
}

/// A pipeline still being built: a source and what has been folded into it so far.
struct Open {
    source: Source,
    filters: Vec<Expr>,
    /// The current columns, as expressions over the source's.
    exprs: Vec<Expr>,
    columns: Vec<Column>,
}

impl Open {
    fn over(source: Source) -> Open {
        let columns = source.columns().to_vec();
        let exprs =
            columns.iter().enumerate().map(|(i, c)| Expr::column(i, c.ty.clone())).collect();
        Open { source, filters: Vec::new(), exprs, columns }
    }

    fn is_identity(&self) -> bool {
        self.exprs.len() == self.source.columns().len()
            && self.exprs.iter().enumerate().all(|(i, e)| e.kind == Kind::Column(i))
    }
}

impl Graph {
    fn push(&mut self, stage: Stage) -> Open {
        let columns = stage.columns().to_vec();
        self.stages.push(stage);
        Open::over(Source::Stage { stage: self.stages.len() - 1, columns })
    }

    /// Ends a pipeline with a result sink, and returns its stage.
    fn close(&mut self, open: Open) -> usize {
        if let Source::Stage { stage, .. } = open.source {
            if open.filters.is_empty() && open.is_identity() {
                return stage;
            }
        }
        let sink = Sink::Result { exprs: open.exprs, columns: open.columns };
        self.stages.push(Stage::Pipeline(Pipeline {
            source: open.source,
            filters: open.filters,
            sink,
        }));
        self.stages.len() - 1
    }

    fn open(&mut self, rel: &Rel) -> Open {
        match rel {
            Rel::Scan { node, table, columns } => Open::over(Source::Scan {
                node: *node,
                table: table.clone(),
                columns: columns.clone(),
            }),
            Rel::Values { rows, columns } => {
                Open::over(Source::Values { rows: rows.clone(), columns: columns.clone() })
            }
            Rel::Filter { input, predicate } => {
                let mut open = self.open(input);
                let predicate = predicate.substitute(&open.exprs);
                conjuncts(predicate, &mut open.filters);
                open
            }
            Rel::Project { input, exprs, columns } => {
                let mut open = self.open(input);
                open.exprs = exprs.iter().map(|e| e.substitute(&open.exprs)).collect();
                open.columns = columns.clone();
                open
            }
            Rel::Aggregate { input, groups, aggregates, columns } => {
                let open = self.open(input);
                let groups = groups.iter().map(|g| g.substitute(&open.exprs)).collect();
                let aggregates = aggregates
                    .iter()
                    .map(|a| Aggregate {
                        name: a.name.clone(),
                        args: a.args.iter().map(|x| x.substitute(&open.exprs)).collect(),
                        distinct: a.distinct,
                        filter: a.filter.as_ref().map(|f| f.substitute(&open.exprs)),
                        ty: a.ty.clone(),
                    })
                    .collect();
                let sink = Sink::Aggregate { groups, aggregates, columns: columns.clone() };
                let p = Pipeline { source: open.source, filters: open.filters, sink };
                self.push(Stage::Pipeline(p))
            }
            Rel::Sort { input, keys } => {
                let open = self.open(input);
                let columns = open.columns.clone();
                let input = self.close(open);
                self.push(Stage::Sort { input, keys: keys.clone(), columns })
            }
            Rel::TopN { input, keys, count, offset } => {
                let open = self.open(input);
                let columns = open.columns.clone();
                let input = self.close(open);
                self.push(Stage::TopN {
                    input,
                    keys: keys.clone(),
                    count: *count,
                    offset: *offset,
                    columns,
                })
            }
            Rel::Limit { input, count, offset } => {
                let open = self.open(input);
                let columns = open.columns.clone();
                let input = self.close(open);
                self.push(Stage::Limit { input, count: *count, offset: *offset, columns })
            }
            Rel::Fetch { input, node, row, columns } => {
                let open = self.open(input);
                let input = self.close(open);
                self.push(Stage::Fetch {
                    input,
                    node: *node,
                    row: row.clone(),
                    columns: columns.clone(),
                })
            }
        }
    }
}

fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e.kind {
        Kind::And(children) => children.into_iter().for_each(|c| conjuncts(c, out)),
        kind => out.push(Expr { kind, ty: e.ty }),
    }
}

impl fmt::Display for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, s) in self.stages.iter().enumerate() {
            match s {
                Stage::Pipeline(p) => {
                    let from = match &p.source {
                        Source::Scan { table, .. } => format!("scan {table}"),
                        Source::Values { rows, .. } => format!("{} literal rows", rows.len()),
                        Source::Stage { stage, .. } => format!("stage {stage}"),
                    };
                    let to = match &p.sink {
                        Sink::Result { exprs, .. } => format!("{} columns out", exprs.len()),
                        Sink::Aggregate { groups, aggregates, .. } => {
                            format!(
                                "aggregate by {} keys into {} accumulators",
                                groups.len(),
                                aggregates.len()
                            )
                        }
                    };
                    writeln!(
                        f,
                        "stage {i}: pipeline from {from}, {} filters, {to}",
                        p.filters.len()
                    )?;
                }
                Stage::Sort { input, keys, .. } => {
                    writeln!(f, "stage {i}: sort stage {input} by {} keys", keys.len())?
                }
                Stage::TopN { input, count, offset, .. } => {
                    writeln!(f, "stage {i}: top {count} offset {offset} of stage {input}")?;
                }
                Stage::Limit { input, count, offset, .. } => match count {
                    Some(n) => {
                        writeln!(f, "stage {i}: limit {n} offset {offset} of stage {input}")?
                    }
                    None => writeln!(f, "stage {i}: offset {offset} of stage {input}")?,
                },
                Stage::Fetch { input, .. } => {
                    writeln!(f, "stage {i}: fetch the rows stage {input} names")?
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudb_plan::Plan;

    fn graph(text: &str) -> Graph {
        let plan = Plan::parse(text).expect("the test plan parses");
        split(&rudb_qc_plan::lower(&plan).expect("the test plan lowers"))
    }

    #[test]
    fn filters_and_projections_fold_into_the_pipeline_before_the_breaker() {
        let g = graph(concat!(
            "TopN 10 offset 0 [#2.1::BIGINT DESC NULLS LAST]\n",
            "  Project #2 [#1.0::VARCHAR AS SearchPhrase, #1.1::BIGINT AS c]\n",
            "    Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]\n",
            "      Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
            "        Get memory.main.hits AS hits #0 [SearchPhrase::VARCHAR]\n",
        ));
        assert_eq!(g.stages.len(), 2, "{g}");
        let Stage::Pipeline(agg) = &g.stages[0] else { panic!("{g}") };
        assert_eq!(agg.filters.len(), 1);
        assert!(matches!(agg.sink, Sink::Aggregate { .. }));
        // The projection over the aggregate only renames, so the top N reads the aggregate.
        assert!(matches!(g.stages[1], Stage::TopN { input: 0, .. }));
        assert_eq!(g.columns()[0].name, "SearchPhrase");
    }

    #[test]
    fn a_filter_above_a_projection_reads_the_source() {
        let g = graph(concat!(
            "Filter (#1.0::INTEGER > 3::INTEGER)::BOOLEAN\n",
            "  Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS y]\n",
            "    Get memory.main.a AS a #0 [x::INTEGER]\n",
        ));
        let Stage::Pipeline(p) = &g.stages[0] else { panic!("{g}") };
        let Kind::Compare { left, .. } = &p.filters[0].kind else { panic!("{g}") };
        assert!(matches!(&left.kind, Kind::Function { name, .. } if name == "+"));
        assert_eq!(left.columns(), [0]);
    }
}
