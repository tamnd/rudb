# Security policy

## Supported versions

The project is pre-1.0 and nothing is supported in the sense that word usually carries. Fixes go on the default branch. Once there is a 1.0, this section will say something more useful.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository, under Security, Report a vulnerability. That opens a private thread with the maintainers. Please do not open a public issue for something you believe is exploitable.

Expect an acknowledgement within a few days. If you have not heard anything in a week, feel free to nudge in a public issue without describing the problem.

## What counts

A database file is untrusted input. People open files they were sent, files from a shared bucket, and files written by an older version of something. A query is untrusted input too, wherever an application puts user text into one. So all of these are in scope:

- Memory unsafety on any input, valid or not. The `unsafe` in this codebase is in five crates: the buffer manager, the vector layer, the kernels, the extension host and the C API. A malformed page that gets a pointer past the end of a mapping is the most serious kind of bug we can have, and the format reader is written on the assumption that every field on disk is hostile.
- A hang or unbounded memory growth on a bounded input. A file or a query that makes the engine consume a machine is a denial of service with extra steps, and the spilling and admission control in [`spec/07-execution.md`](spec/07-execution.md) are what is supposed to prevent it.
- Anything that lets a query read data outside the database it was asked to read, or write outside the files it was asked to write, or execute something it was not asked to execute. The extension loading path in [`spec/13-ecosystem.md`](spec/13-ecosystem.md) is the obvious place for that and it is not the only one.
- A path through the C API where a valid sequence of calls from a caller who followed the header produces unsoundness. The header is the contract, and if the contract can be honored and still lose, the contract is wrong.

A controlled error on a corrupt file is not a vulnerability. That is the intended behavior: [`spec/16-testing.md`](spec/16-testing.md) section 16.7 requires a clean error, never a panic inside `unsafe`, and never a hang.

A wrong query answer is not usually a security problem, but it is the most serious class of correctness defect in the project and it has its own issue template. File it publicly unless you think it can be induced by an attacker who controls part of the query.

## What we do about it

Reports are triaged, reduced, fixed on a private branch, and released with an advisory that says what the problem was and what it affected. Reporters are credited unless they prefer not to be.
