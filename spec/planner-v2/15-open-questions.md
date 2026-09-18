# Open questions

Eight things this folder does not settle. Each one says what the question is, what is known, what would settle it, and what the rest of the folder assumed in the meantime so that the assumption is findable when the answer arrives.

## 15.1 Are the physical plan and the pipeline program two artifacts or one

**The question.** Document 03 separates artifact 5 from artifact 6 on the grounds that one names implementations and the other names blocks. A reasonable reading is that this is one artifact with two printers, and that the physical plan is just the program before the state declarations are computed.

**What is known.** The separation earns its keep in two places. The physical plan is where cost lives and the program must not have a cost. And the physical plan is a tree while the program is a list, which is a real difference because a tree is what a cost comparison recurses over and a list is what a compiler walks.

**What would settle it.** Building both and counting how many lowering rules are one to one. If most physical nodes lower to a fixed block sequence with no choice in it, the lowering is a mechanical expansion and the two should collapse.

**What was assumed.** Two artifacts, because collapsing later is easier than splitting later.

## 15.2 What an `Exact` fact means under concurrent writes

**The question.** Document 04 section 4.3 lets an enabling rewrite fire only on an `Exact` fact, because a wrong fact there gives a wrong answer rather than a slow query. `Fact` carries a provenance but the design does not say which generation of the data the fact is exact for, or what happens when a statement's snapshot and the fact's generation differ.

**What is known.** `../stats/` keys observations by generation and `../stats/04-in-memory.md` binds a statement to a snapshot key, so the information exists. What is not specified is the rule. A distinct count that was exact two transactions ago is not exact now, and a `DISTINCT` elimination that fired on it is wrong rather than slow.

**What would settle it.** A stated rule of the form: a fact may be read as `Exact` only when its generation matches the statement snapshot, and otherwise it degrades to `Certified` with a bound derived from the number of rows written since, or to `Unknown`. The degradation is the part to get right, because degrading everything to `Unknown` on the first write makes enabling rewrites useless on a live database.

**What was assumed.** That the generation check exists and that a mismatched fact is not read as `Exact`. This is the most load bearing unstated assumption in the folder and it should be the first thing written down after it.

## 15.3 Shared concurrent table against partitioned, on the hardware that matters

**The question.** Document 11 section 11.3 specifies both and leaves the choice to the physical planner. Which one wins where, on rudb's actual targets, is unmeasured.

**What is known.** Document 02 section 2.6's evidence is that a shared table with a good ticketing scheme is now competitive and that the old assumption favouring partitioning was hardware that has moved. The cases are also well understood in theory: shared wins on high cardinality with low skew, partitioned wins on low cardinality and under memory pressure.

**What would settle it.** The strategy differential harness from document 13 section 13.3, which forces each strategy over the corpus, run on an x86 server, an ARM server and a laptop. That harness has to exist for correctness reasons anyway, so the measurement is nearly free once P5 lands.

**What was assumed.** Both exist and the planner picks from a distinct count and a reservation. No threshold is stated in this folder on purpose.

## 15.4 How far the link coverage extends

**The question.** Document 06's strong claim, that rudb's semi-join reduction is exact rather than approximate, holds only on an edge where `../graph/` has a link. TPC-H is full of such edges. JOB and the Cardinality Estimation Benchmark are joins over a real schema with many more of them declared than built, and a query over Parquet has none.

**What is known.** The fallback is specified and is the ordinary Bloom path, so the worst case is what other engines do. What is not known is the fraction of edges in a realistic workload that get the strong path, and the whole 10x claim on join workloads is proportional to that fraction.

**What would settle it.** Publishing link coverage per benchmark as a first class number: edges total, edges with a declared relationship, edges with a built link, edges reduced exactly. It is cheap to measure once P6 lands and it should appear next to the timings rather than in a footnote.

**What was assumed.** That coverage is high on TPC-H and unknown elsewhere, which is why document 14 puts TPC-H first in P6's exit criteria.

## 15.5 Is the block set actually closed at thirty

**The question.** Document 08 section 8.3 proposes about thirty blocks and a rule that a new one needs an argument. Window functions, lateral joins, recursive CTEs, set operations and list and struct types are the five things most likely to break that.

**What is known.** `window.rs` is 1,251 lines and a frame boundary computation is genuinely unlike anything else in the set, so it will need at least one block of its own. Recursive CTEs need a loop at the pipeline level that the control block set does not describe well.

**What would settle it.** Lowering all six existing operator families during P7 and counting. If the count comes out at forty five, the design is still fine. If it comes out at a hundred and twenty, the block set is an operator list with more steps and the compiler will not be tractable over it.

**What was assumed.** Thirty as a target rather than a limit, with window explicitly granted an exception.

## 15.6 Whether a cost model fitted once travels

**The question.** Document 07 section 7.4 says the weights are fitted once by measurement and pinned, and that the model only ever compares two plans for the same node so absolute miscalibration does not matter. That argument is sound when the miscalibration scales both sides equally. It is not obviously sound across machines whose ratios differ, and the ratio of a random access to a sequential one differs enormously between a laptop with fast storage and a server with slow storage and a large cache.

**What is known.** Nothing measured. The mitigation in the design is the near-tie rule, which takes the documented default rather than the marginally better score, and that rule absorbs some of this.

**What would settle it.** Fitting the weights on three machines and checking whether any plan in the corpus changes. If none does, the question is closed and the model is robust. If several do, either the model needs a machine profile or those decisions need to be made from something other than cost.

**What was assumed.** One fitted set of weights, pinned, with the near-tie rule doing the work.

## 15.7 What happens on data with no statistics at all

**The question.** The folder's answer to a bad plan is a better fact, and its refusal of mid-query re-optimization in document 12 section 12.4 rests on that. A query over foreign Parquet files, a CSV just attached, or a table written seconds ago has no synopses, no links and possibly no row counts beyond a footer.

**What is known.** `Facts::get` returns `Unknown`, the decisions fall back to documented defaults, and the plan is printed with its guesses visible. That is honest and it is also exactly the situation where an adaptive engine wins and rudb does not.

**What would settle it.** Measuring the gap. Run the benchmark corpus against Parquet inputs with no statistics and compare to DuckDB, which has the same problem and solves it partly by sampling at bind time. If the gap is small, the position holds. If DuckDB is meaningfully faster there, the answer is more likely to be cheap sampling at bind time than mid-query re-planning, and that is a smaller change than reopening document 12.

**What was assumed.** That the native format is the case that matters for the performance claim and the foreign format case needs only to not be embarrassing.

## 15.8 How far encoded execution goes before it is a second engine

**The question.** Document 10 section 10.4 names four encoded operations. Each one is a kernel that exists in addition to the raw one, and the number of kernels is operations times encodings times types. There is a point at which the encoded paths are a second implementation of the engine with its own bugs, and the folder does not say where it is.

**What is known.** The four chosen operations are the high value ones and the rule that an unmet requirement is a decode rather than an error keeps the fallback always available. The kernel generator from `../07-execution.md` section 7.3 generates from a table, which makes the multiplication cheaper than writing them by hand.

**What would settle it.** A budget, stated as a number of generated kernels and a share of test time, plus a rule about which combinations are worth generating. The honest version is probably that encodings get encoded paths only for the operations that appear on the benchmark's hot columns, and everything else decodes.

**What was assumed.** Four operations, generated rather than written, with the decode fallback always present.

## What we should take from this document

Two of these are more urgent than the rest. Section 15.2, what an `Exact` fact means under concurrent writes, is a correctness question and the enabling rewrites in P1 depend on the answer, so it should be settled before they ship. Section 15.4, link coverage outside TPC-H, decides how large the largest claim in the folder actually is, and it is cheap to measure.

The other six are all questions that a measurement answers once the corresponding phase lands, which is the point of writing them down here rather than guessing at an answer now.
