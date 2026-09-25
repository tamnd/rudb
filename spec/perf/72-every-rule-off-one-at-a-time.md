# Every rule off, one at a time

G4 in tamnd/rudb#764 asks for a table with one row per statistics rule on ClickBench, and for claim S5 of [`spec/stats/09-measurement.md`](../stats/09-measurement.md), which is the claim that the statistics make queries fast that have no joins in them. Both need one thing the tree did not have: a way to run one binary against itself with one rule turned off. The rules have had their own settings since `crates/rudb-common/src/rules.rs` was written, and `spec/stats/09-measurement.md` section 9.2 is why they do, but the instrument that reads them was built to compare two binaries and there was nothing to put a setting on one side of it.

So this is two things. The first is `--ablate` in `scripts/instructions`, which is the apparatus. The second is what it says when it is pointed at the four rules G4 names, which is that none of them is worth a percent on ClickBench, that the layer as a whole is worth one percent in the session and slightly negative in the protocol, and that the protocol is the more interesting half of that sentence.

Everything below is server2, one thread, main at `905a71e0` built for `x86-64-v3`, over a hits corpus of 999,975 rows, counted at ring 3 as [`68-the-counter-that-was-not-load-immune.md`](68-the-counter-that-was-not-load-immune.md) requires.

## What the harness gained

`--ablate NAME` runs the baseline side with a setting off and the side under test with it on, so `--after` can be left out and one build measured against itself. A ratio under 1.000x is the rule earning something. `--before-set` and `--after-set` are the general form for a switch whose two interesting values are not on and off, `--except qNN` leaves one query out of a suite without giving up the answer check on the rest, and `--plans` runs `EXPLAIN` on both sides and marks the queries whose plan came out different.

The reason `--ablate` gives a setting to each side rather than giving the off side a switch the on side has not got is measured rather than assumed:

| ClickBench, 43 queries | suite | the query that moved |
| --- | --- | --- |
| one extra `SET` on one side, value off | 1.000x | q12 1.011x |
| one extra `SET` on one side, value on, which changes nothing | 1.000x | q12 1.009x |
| the same setting on both sides, same value | 1.000x | nothing above 1.001x |

An extra `SET` is about 0.7 M instructions at start up, and the `SELECT 1` subtraction takes that back out. What it does not take out is whatever it did to q12, which reads 1.009x with the setting at its default value and is therefore not the setting doing anything. One value of one switch against the other value of the same switch reads clean on all 43, so that is the protocol and `--ablate` is what spells it.

## The four rules

Each row is one run of 42 queries, the rule off against the rule on, three rounds a query, one query per process. Under 1.000x is the rule winning. q8 is left out and the last section says why.

| rule | plan changed on | median there | median elsewhere | worst elsewhere |
| --- | --- | --- | --- | --- |
| `stats.presize` | 11 queries | 1.000x | 1.000x | q41 1.005x |
| `stats.direct_addressing` | nothing | | 1.000x | q28 0.968x |
| `stats.validity_free` | q30 | 1.011x | 1.000x | q20 1.012x |
| `stats.filter_order` | q22 q23 | 0.999x | 1.000x | q37 1.003x |
| `stats.all`, the four above and five more | 34 queries | 1.003x | 1.000x | q1 1.086x |

Presizing an aggregate from the distinct count of its key changes the plan of eleven ClickBench queries and the instruction count of none of them. Direct addressing is the only one that wins anything, 3.2 percent on q28, which groups by `CounterID` over a million rows. The validity free rewrite costs 1.1 percent on the one query it rewrites. Conjunct ordering fires on two and is worth a tenth of a percent on them.

The everything else column is a tenth of a percent rather than nothing, and it is the same seven queries every time. q37 to q43 are the cheapest queries in the suite that still read rows, between five and eleven million instructions each, and each of them reads between 1.003x and 1.005x under presizing and under conjunct ordering alike. That is tens of thousands of instructions for a rule to ask a question about a query it then cannot do anything for, which is what section 9.4 of `spec/stats/09-measurement.md` predicted when it said the everything else column can come out negative.

The layer as a whole reads 1.001x, which is to say the engine does very slightly more work with the statistics on than with them off, over 42 of the 43 ClickBench queries, in this protocol.

## Two things that table gets wrong

**A plan diff under reports which queries a rule reached.** Direct addressing changes the printed plan of no query in the 42 and moves q28 by 3.2 percent. Presizing writes a group count onto the aggregate and `EXPLAIN` prints it; the dense pass writes a key range onto the same node and `EXPLAIN` does not. So the `--plans` column is a lower bound on firing and it is worth having anyway, because a query whose plan changed and whose count did not is a rule that costs nothing where it fires, and a query whose count changed and whose plan did not is a rule reaching the executor through something nobody can read.

**One query per process charges a once per process cost to every query.** The whole per rule table above is 42 processes a side, and each of those processes is the first thing that ever asked this table a question. Run the same 43 queries as `EXPLAIN` so that nothing executes and only the planner runs:

| ClickBench, plan only | statistics off | statistics on | |
| --- | --- | --- | --- |
| q20 | 0.6 M | 2.1 M | 3.648x |
| q26 | 0.7 M | 1.8 M | 2.565x |
| q38 | 1.6 M | 3.2 M | 1.964x |
| q28 | 1.5 M | 2.6 M | 1.694x |
| q43 | 1.9 M | 2.6 M | 1.349x |
| all 43 | 0.09 G | 0.11 G | 1.248x |

That is about 1.1 M to 1.5 M instructions of planning a query, and on q20, which reads one column and returns one row, it is more than the query. Now ask the same question in one process instead of 300 of them, by running q20's `EXPLAIN` three hundred times down standard input: 502.0 M with the statistics on and 466.9 M with them off, which is 0.117 M a statement. So of the 1.5 M, about 1.4 M is paid once and the rest is what each plan actually spends asking. The per rule table pays that 1.4 M forty two times over and then reports it as the rules being slightly negative.

## The suite in one session, which is how ClickBench is run

ClickBench is 43 queries against one table in one session. Measured that way, minimum of three runs of the whole file:

| ClickBench, one session, 43 queries | instructions | |
| --- | --- | --- |
| as the engine comes | 5.541 G | |
| `stats.all = off` | 5.601 G | the statistics are worth 1.1 percent |
| `stored.answers = off` | 5.969 G | what the rules of the benchmark require |
| `stored.answers = off` and `stats.all = off` | 6.089 G | the statistics are worth 2.0 percent |

Two numbers worth keeping out of that. The statistics layer is worth 1.1 percent of ClickBench as the engine comes and 2.0 percent of a run that is allowed to count, and it is worth more on the legal run because a whole table aggregate answered out of the file header is an aggregate no rule can improve. And stored answers are worth 7.2 percent, which is 428 M instructions, all of it on the seven queries that ask for a count or a sum over the whole table, and none of it allowed.

## Claim S5

**Killed.** The claim is that the statistics make queries fast that have no joins in them, and it says the way to kill it is the ClickBench total not moving. It moves by 1.1 percent in the session and by 0.1 percent the wrong way in the per query protocol. One percent is not the claim.

The diagnosis is not that the rules are wrong, it is that ClickBench gives them nothing to decide. There are no joins to order, no join to eliminate, one table to scan and no choice about scanning it. What is left for a statistic to do is size a hash table, pick direct addressing over hashing, drop a null check and order two conjuncts, and the first, third and fourth of those are worth nothing measurable here while the second is worth 3.2 percent on one query. The claim was written expecting the opposite, which is what makes it worth having written down.

Two things follow. The engine's ClickBench number has to be earned in the kernels and the scan rather than in the planner, which is where [`68-the-counter-that-was-not-load-immune.md`](68-the-counter-that-was-not-load-immune.md) and the notes around it have been spending their effort, and that is the right place to keep spending it. And the statistics layer has to stop costing 1.4 M instructions the first time a query touches a table, because section 9.6 of `spec/stats/09-measurement.md` asks for exactly that number under the name cold open and an embedded database runs an enormous number of small queries. 1.4 M is three trivial queries' worth, it is paid on a table with 105 columns for a query that mentions one of them, and section 11.2 of `spec/stats/11-open-questions.md` is where the decision that allows it is written down.

## Section 9.4 asks for a threshold rather than a shrug

It is the right ask and the answer here is that there is nothing yet to put a threshold on. The two negative readings in the table, `stats.validity_free` at 1.011x on q30 and at 1.012x on q20, are both the first touch cost above rather than the rewrite being a bad idea: q30's whole regression reproduces with `EXPLAIN` alone, which executes nothing, and q20's plan does not change at all. A threshold on a rule that costs nothing where it fires would be a threshold on the wrong thing. The number to attack is the once per table one, and when it is gone this table should be run again, because a rule that is genuinely negative cannot be told from one that is standing next to something else's cost until then.

## q8 and the tie

`SELECT AdvEngineID, COUNT(*) FROM hits WHERE AdvEngineID <> 0 GROUP BY AdvEngineID ORDER BY COUNT(*) DESC` has two groups with ten rows each and no tiebreak, so hashing and direct addressing put them in the two different orders that the query permits and the harness stops the run rather than measure two different answers. That is the harness being right. Section 9.3 of `spec/stats/09-measurement.md` already says answers must match modulo the tie rules that `spec/bench/tpc-h/04-the-answers.md` section 4.4 defines for a non-total order, so `--except q8` is how one query with a tie gets left out while the other 42 keep their check, and the alternative, `--skip-answers`, gives up the check that has caught things.
