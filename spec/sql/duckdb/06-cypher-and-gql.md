# The graph front end

Nothing in this document is scheduled. It is here because document 04 claims the seam is general and a claim like that is only worth something if somebody has worked out what the hardest language would actually cost.

## 6.1 Two standards that are one pattern language

SQL/PGQ is part 16 of ISO 9075, published in 2023, and it adds one thing to SQL: a `GRAPH_TABLE` table expression that matches a pattern against a property graph defined over existing tables. GQL is ISO/IEC 39075, published in April 2024, and is a standalone language, the first new ISO database language since SQL itself.

They share a core. Both are built on the same graph pattern matching sublanguage, so the pattern in a SQL/PGQ `GRAPH_TABLE` and the pattern in a GQL `MATCH` are the same grammar with the same semantics, and openCypher's `MATCH` is close enough to that core that the differences are a list rather than a rewrite. Checked 14 September 2026 against the ISO abstracts and the GQL standards site.

That is what makes the sequencing claim in the README true. The expensive part of a graph language is the pattern matcher: parsing a path pattern, binding it against a graph definition, and turning it into an algebra. The cheap part is the surrounding statement syntax. SQL/PGQ and Cypher differ in the cheap part. Build the pattern matcher once, reach it from `GRAPH_TABLE` first because that is inside the SQL surface this folder is about, and reach it from a Cypher dialect second because by then the hard half exists.

## 6.2 The property graph is a view, not a store

SQL/PGQ defines a graph with `CREATE PROPERTY GRAPH`, naming vertex tables, edge tables, and which columns are the keys that connect them. The graph is a definition over tables that already exist. There is no second storage engine, no second catalog and no data duplication, and section 4.6 already committed to that.

DuckPGQ is the existence proof. It implements SQL/PGQ inside DuckDB as an extension, over ordinary DuckDB tables, building a compressed sparse row structure at query time for the path finding operators, and it was published at CIDR 2023 and VLDB. It also uses `ParserExtension`, which is the mechanism section 4.5 keeps. So the arrangement this document proposes is one somebody already shipped against the engine we are matching.

The counter example is Apache AGE, which stores graph data in its own schema and exposes Cypher through `cypher($$ ... $$)` returning `agtype`. Section 4.2 already gave the reason not to: the planner cannot see inside the function, so no predicate crosses the boundary. It ships faster and it is permanently opaque.

## 6.3 What the plan needs that it does not have

Section 5.4 said three of the seven planned additions get reused here. This is the part that is genuinely new.

**Variable length paths need the fixpoint operator with a bound.** `(a)-[*1..5]->(b)` is the recursive CTE operator from section 5.2 with a depth counter and an iteration limit. No new node, different parameters.

**Path semantics are not free.** GQL has restrictors, `TRAIL`, `ACYCLIC` and `SIMPLE`, and selectors, `ALL`, `ANY`, `ALL SHORTEST` and `ANY SHORTEST`. Cypher's `MATCH` defaults to trail semantics, meaning no relationship may repeat within one match, and that default is the reason a naive fixpoint gives wrong answers rather than slow ones. Enforcing it means each intermediate row carries the set of edges used so far and the fixpoint step filters against it. That is a per row growing state, it is the thing that makes graph engines hard, and it is not expressible as a plain relational fixpoint without carrying that state as a value.

**A path is a value.** `RETURN p` where `p` is a matched path returns something that is neither a row nor a scalar. The cheap representation is a `LIST` of identifiers, which the type system already has once section 2.4's composite work is done, and the honest one is a `PATH` type. Section 4.6 says a dialect may not invent a value representation, so this is a `LogicalType` decision made once, for the engine, not for the dialect.

**Shortest path is an operator, not a rewrite.** `ALL SHORTEST` over a fixpoint is a breadth first search with early termination, and expressing it as fixpoint plus a minimum aggregate is correct and computes the whole reachable set first. If graph queries are ever a real workload this is a physical operator with its own implementation.

So the count is honest: one new type, one new physical operator, and a fixpoint that has to carry a per row set. Everything else is shared.

## 6.4 Factorized processing, and why not yet

Kuzu's central technical claim is factorized query processing: intermediate results are kept in a factorized form rather than flattened, which makes many to many joins along paths asymptotically smaller, and it pairs that with worst case optimal joins. The work is published, at VLDB 2021 as arXiv:2103.02284 and at CIDR 2023, and the results are real.

It is also a change to the executor's data model rather than to the plan's. A factorized vector is not a vector of values, it is a nested structure, and every operator in the engine has to understand it or flatten at the boundary. That is a commitment on the scale of the vectorized execution decision itself, and it pays for exactly one workload shape.

The decision here is to keep the flat model and record the reason: rudb's executor and storage exist to be DuckDB compatible, and DuckDB is not factorized. If graph workloads ever justify it, the place it lands is the executor, not the seam, and nothing in documents 04 or 05 would have to change. That is a useful property of putting the seam at the plan rather than lower.

## 6.5 The one that is not obvious

Cypher's grouping is implicit. `RETURN a.name, count(*)` groups by `a.name` with nothing written down, and the rule is that every non aggregated return item is a grouping key. SQL makes you write `GROUP BY`. The formal account of this is in the DBPL 2019 paper on Cypher's semantics, and the general mapping from openCypher to relational algebra is in arXiv:1705.02844.

It matters here for one reason, and it is the reason this document exists inside a compatibility folder rather than beside it. That rule is a binder rule, in a dialect's own binder, producing an ordinary `Aggregate` node. It is exactly the kind of difference that looks like it needs a flag somewhere deep and does not. Every difference between these languages that has been traced in this document lands above the plan, except the three things in section 6.3, and those three are additions to the engine rather than parameters on it.

That is the claim document 04 needed tested, tested against the hardest case available, and it survives.
