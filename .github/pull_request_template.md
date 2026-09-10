## What this changes

<!-- The problem first, then the change. What is in the diff does not need restating. -->

## Why

<!-- If it closes an issue, say `Closes #N`. If it implements part of a milestone, say which. -->

## How it was verified

<!-- Which test fails without this change. If none does, say so and say why. -->

## Checklist

- [ ] `cargo xtask ci` passes locally
- [ ] A change to behavior comes with a test that fails without it
- [ ] A change that could return a different answer comes with a case in the differential corpus
- [ ] A new encoded fast path comes with the equivalence test that runs it against the decoded path on the same data
- [ ] A performance claim comes with the command that reproduces it, median of ten runs with the interquartile range
- [ ] A change to the storage format comes with a format version bump and a round trip test
- [ ] A new dependency comes with a row in the table in `spec/18-package-layout.md`
- [ ] A new `#[ignore]` or corpus exclusion comes with an issue number
- [ ] Prose follows the house rules: plain English, no em dashes, no horizontal rules, no hard-wrapped sentences
