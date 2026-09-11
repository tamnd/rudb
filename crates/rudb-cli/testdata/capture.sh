#!/bin/sh
# Recaptures the golden output in this directory from a real DuckDB binary.
#
# Run it from anywhere with `duckdb` on the path. It overwrites every mode-*.txt file and rewrites
# duckdb-version.txt, so the version recorded there is always the version that produced the bytes
# beside it. The shell test in tests/shell.rs diffs against these, which is the only reason any of
# the claims in src/format.rs about DuckDB are worth believing.
#
# The `duckdb` it wants is the pinned one, which `scripts/oracle` installs and which no package
# manager has. That means one of the machines rather than a laptop, and it means the files in here
# are Linux output: `.mode csv` ends its lines with a carriage return and a newline there.
set -e
here=$(cd "$(dirname "$0")" && pwd)
duckdb -version >"$here/duckdb-version.txt"
for mode in duckbox box table markdown line list csv tabs json jsonlines quote insert html ascii column; do
  duckdb -batch -init /dev/null \
    -cmd ".read $here/setup.sql" \
    -cmd ".mode $mode" \
    -c "SELECT * FROM t ORDER BY a" >"$here/mode-$mode.txt"
done
duckdb -batch -init /dev/null -cmd ".read $here/setup.sql" -c "SELECT * FROM t WHERE a > 9000" >"$here/empty.txt"
# The one query that covers every byte the CSV writer puts quotes around.
duckdb -batch -init /dev/null -cmd ".mode csv" -c ".read $here/quoting.sql" >"$here/quoting.txt"
# The same query through the flag rather than the dot command, because the flag does not set the row
# separator and the two files differ by exactly that.
duckdb -batch -init /dev/null -csv -c ".read $here/quoting.sql" >"$here/quoting-flag.txt"
duckdb -batch -init /dev/null -c ".show" >"$here/show.txt"
# What each mode flag leaves the settings at, which is not what the dot command of the same name
# leaves them at and is not the same answer for every flag. The separators are set to something odd
# first so that a flag which leaves one alone can be told apart from one that sets it to the value
# it already had.
for mode in ascii box column csv html json jsonlines line list markdown quote table; do
  duckdb -batch -init /dev/null -separator ';' -newline '@' "-$mode" -c ".show" >"$here/flag-$mode.txt"
done
# Which of the mode names the binary has no flag for, probed rather than read off the help text,
# because the help text is prose and this is what the parser does. Four of the sixteen modes are not
# flags, and neither are the three aliases, so `.mode tabs` works and `-tabs` is an error.
: >"$here/flags-refused.txt"
for mode in ascii box column csv duckbox html insert json jsonlines line lines list markdown ndjson quote table tabs trash tsv; do
  duckdb -batch -init /dev/null "-$mode" -c "SELECT 1" >/dev/null 2>&1 || echo "$mode" >>"$here/flags-refused.txt"
done
# One file per line of counts.sql, which is where the row count and the column count and the row of
# dots get their shapes. The test reads the same file, so a query added here needs nothing else.
at=0
while IFS= read -r query; do
  at=$((at + 1))
  duckdb -batch -init /dev/null -c "$query" >"$here/counts-$at.txt"
done <"$here/counts.sql"
